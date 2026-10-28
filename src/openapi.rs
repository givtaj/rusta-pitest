//! Deterministic, inert OpenAPI 3.1 contract-to-suite generation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    time::Duration,
};

use reqwest::{Client, Url, redirect, retry};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    model::{Case, Parameter, Source, Suite},
    schema,
    strict_json::{self, MAX_CONFIGURED_BYTES},
};

const METHODS: [&str; 8] = [
    "get", "head", "post", "put", "patch", "delete", "options", "trace",
];
type Security = Vec<BTreeMap<String, Vec<String>>>;

/// Load only the requested contract document. No operation, reference, example,
/// authentication, discovery, or documentation-page requests are made here.
pub async fn generate(source: &str, max_input_bytes: u64) -> Result<Suite, String> {
    if !(1..=MAX_CONFIGURED_BYTES).contains(&max_input_bytes) {
        return Err("contract input limit must be between 1 byte and 64 MiB".into());
    }
    let bytes = load(source, max_input_bytes).await?;
    let start = bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    if start == Some(b'<') {
        return Err(
            "received HTML; supply the raw OpenAPI JSON/YAML URL, not a Swagger UI page".into(),
        );
    }
    let contract = if matches!(start, Some(b'{' | b'[')) {
        strict_json::parse_json_with_limit(&bytes, max_input_bytes as usize)?
    } else {
        strict_json::parse_yaml_with_limit(&bytes, max_input_bytes as usize)?
    };
    let openapi_version = version(&contract)?.to_owned();
    let source = Source {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        openapi_version,
    };
    from_contract(contract, source)
}

async fn load(source: &str, max_input_bytes: u64) -> Result<Vec<u8>, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let url = Url::parse(source).map_err(|_| "invalid contract URL")?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(
                "contract URL credentials are unsupported; use a local contract file".into(),
            );
        }
        if url.fragment().is_some() {
            return Err("contract URL must identify a document without a fragment".into());
        }
        let client = Client::builder()
            .http1_only()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .retry(retry::never())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "cannot initialize contract HTTP client")?;
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|_| "cannot fetch contract document")?;
        if !response.status().is_success() {
            return Err(
                "contract fetch did not return a successful status; redirects are disabled".into(),
            );
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_input_bytes)
        {
            return Err("contract document exceeds the configured input limit".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "cannot read contract response body")?
        {
            if bytes.len().saturating_add(chunk.len()) as u64 > max_input_bytes {
                return Err("contract document exceeds the configured input limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    } else {
        if source.contains("://") {
            return Err("contract source must be an HTTP(S) URL or a local filesystem path".into());
        }
        let file = File::open(source).map_err(|_| "cannot open contract file")?;
        let mut bytes = Vec::new();
        file.take(max_input_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read contract file")?;
        if bytes.len() as u64 > max_input_bytes {
            return Err("contract document exceeds the configured input limit".into());
        }
        Ok(bytes)
    }
}

fn version(contract: &Value) -> Result<&str, String> {
    let version = contract
        .get("openapi")
        .and_then(Value::as_str)
        .ok_or("expected an OpenAPI 3.1 document with an openapi version field")?;
    if !matches!(version, "3.1.0" | "3.1.1" | "3.1.2") {
        return Err(
            "unsupported OpenAPI version; this importer supports 3.1.0, 3.1.1 and 3.1.2".into(),
        );
    }
    Ok(version)
}

/// Rebuild the deterministic plan from a frozen contract. The runner uses this
/// before execution so hand editing a suite cannot bypass generation policies.
pub fn from_contract(contract: Value, source: Source) -> Result<Suite, String> {
    if version(&contract)? != source.openapi_version {
        return Err("suite source version does not match the frozen contract".into());
    }
    if source.sha256.len() != 64 || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("suite source hash must be a SHA-256 hexadecimal digest".into());
    }
    let root = contract.as_object().ok_or("contract must be an object")?;
    let info = root
        .get("info")
        .and_then(Value::as_object)
        .ok_or("contract info must be an object")?;
    for field in ["title", "version"] {
        if !info.get(field).is_some_and(Value::is_string) {
            return Err("contract info requires string title and version fields".into());
        }
    }
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("contract paths must be an object")?;
    let inherited_security = parse_security(root.get("security"))?;
    let mut operation_ids = BTreeSet::new();
    let mut cases = Vec::new();
    for (path, path_item) in paths {
        if path.starts_with("x-") {
            continue;
        }
        if !path.starts_with('/') || path.chars().any(char::is_control) || path.contains(['?', '#'])
        {
            return Err("contract paths must start with / and contain no control characters, query or fragment".into());
        }
        if let Some(object) = path_item.as_object()
            && object.contains_key("$ref")
            && object.keys().any(|key| {
                !matches!(key.as_str(), "$ref" | "summary" | "description")
                    && !key.starts_with("x-")
            })
        {
            return Err(
                "path-item references with operation, parameter or server siblings are unsupported"
                    .into(),
            );
        }
        let path_item = schema::resolve_path_item(&contract, path_item)
            .map_err(|_| "path-item references must resolve inside the contract document")?;
        let item = path_item.as_object().ok_or("path item must be an object")?;
        let inherited_parameters = parameter_map(&contract, item.get("parameters"))?;
        let path_names = path_parameter_names(path)?;
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let operation = operation.as_object().ok_or("operation must be an object")?;
            let id = match operation.get("operationId") {
                Some(Value::String(id)) if !id.is_empty() && !id.chars().any(char::is_control) => {
                    id.clone()
                }
                Some(_) => {
                    return Err(
                        "operationId must be a nonempty string without control characters".into(),
                    );
                }
                None => format!("{} {path}", method.to_uppercase()),
            };
            if !operation_ids.insert(id.clone()) {
                return Err("contract operation IDs must be unique".into());
            }
            let mut case = Case {
                id,
                method: method.to_uppercase(),
                path: path.clone(),
                parameters: Vec::new(),
                security: if operation.contains_key("security") {
                    parse_security(operation.get("security"))?
                } else {
                    inherited_security.clone()
                },
                responses: operation
                    .get("responses")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                body: None,
                blockers: Vec::new(),
            };
            if !matches!(method, "get" | "head") {
                block(
                    &mut case,
                    "operation execution is restricted to selected GET/HEAD cases",
                );
            }
            if let Some(dialect) = contract.get("jsonSchemaDialect")
                && dialect.as_str() != Some("https://spec.openapis.org/oas/3.1/dialect/base")
                && dialect.as_str() != Some("https://json-schema.org/draft/2020-12/schema")
            {
                block(&mut case, "custom JSON Schema dialect is unsupported");
            }
            let mut parameters = inherited_parameters.clone();
            match parameter_map(&contract, operation.get("parameters")) {
                Ok(overrides) => parameters.extend(overrides),
                Err(_) => block(&mut case, "operation parameters are invalid or unresolved"),
            }
            for ((location, name), parameter) in parameters {
                match make_parameter(&contract, &case.id, &location, &name, &parameter) {
                    Ok(parameter) => case.parameters.push(parameter),
                    Err(reason) => block(&mut case, &reason),
                }
            }
            for name in &path_names {
                if !case.parameters.iter().any(|parameter| {
                    parameter.location == "path" && parameter.name == *name && parameter.required
                }) {
                    block(
                        &mut case,
                        "path template requires a matching required path parameter",
                    );
                }
            }
            if case.parameters.iter().any(|parameter| {
                parameter.location == "path" && !path_names.contains(&parameter.name)
            }) {
                block(
                    &mut case,
                    "declared path parameter is absent from the path template",
                );
            }
            validate_security(&contract, &mut case);
            validate_responses(&contract, &mut case);
            if let Some(body) = operation.get("requestBody") {
                if matches!(method, "get" | "head") {
                    block(&mut case, "GET/HEAD request bodies are unsupported");
                }
                match schema::resolve(&contract, body) {
                    Ok(body) => {
                        if let Some(body_schema) = body.pointer("/content/application~1json/schema")
                        {
                            case.body = synthesize(&contract, body_schema);
                            if case.body.is_none() {
                                block(&mut case, "cannot synthesize a supported JSON request body");
                            }
                        } else {
                            block(
                                &mut case,
                                "request body requires a supported application/json schema",
                            );
                        }
                    }
                    Err(_) => block(
                        &mut case,
                        "request body reference is unresolved or external",
                    ),
                }
            }
            case.blockers.sort();
            case.blockers.dedup();
            cases.push(case);
            if cases.len() > 256 {
                return Err("contract exceeds the 256-operation generation limit".into());
            }
        }
    }
    cases.sort_by(|left, right| (&left.path, &left.method).cmp(&(&right.path, &right.method)));
    if cases.is_empty() {
        return Err("contract contains no supported HTTP operation definitions".into());
    }
    Ok(Suite {
        suite_version: 1,
        source,
        contract,
        cases,
    })
}

fn block(case: &mut Case, reason: &str) {
    case.blockers.push(reason.to_owned());
}

fn parse_security(value: Option<&Value>) -> Result<Security, String> {
    value.map_or_else(
        || Ok(Vec::new()),
        |value| {
            serde_json::from_value(value.clone()).map_err(|_| {
                "security requirements must be an array of scheme-to-scope objects".into()
            })
        },
    )
}

fn parameter_map(
    contract: &Value,
    value: Option<&Value>,
) -> Result<BTreeMap<(String, String), Value>, String> {
    let mut parameters = BTreeMap::new();
    if let Some(value) = value {
        let list = value.as_array().ok_or("parameters must be an array")?;
        if list.len() > 128 {
            return Err("parameter list exceeds the supported 128-parameter limit".into());
        }
        for parameter in list {
            let parameter = schema::resolve(contract, parameter)
                .map_err(|_| "parameter reference is unresolved or external")?;
            let location = parameter
                .get("in")
                .and_then(Value::as_str)
                .ok_or("parameter requires a location")?;
            let name = parameter
                .get("name")
                .and_then(Value::as_str)
                .ok_or("parameter requires a name")?;
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err("parameter name must be nonempty without control characters".into());
            }
            if parameters
                .insert((location.to_owned(), name.to_owned()), parameter)
                .is_some()
            {
                return Err("duplicate parameter name and location".into());
            }
        }
    }
    Ok(parameters)
}

fn path_parameter_names(path: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let mut current = None;
    for character in path.chars() {
        match character {
            '{' if current.is_none() => current = Some(String::new()),
            '}' => {
                let name = current.take().ok_or("unbalanced path template")?;
                if name.is_empty() {
                    return Err("path template parameter must be nonempty".into());
                }
                names.insert(name);
            }
            '{' => return Err("nested path templates are unsupported".into()),
            character => {
                if let Some(name) = &mut current {
                    name.push(character);
                }
            }
        }
    }
    if current.is_some() {
        return Err("unbalanced path template".into());
    }
    Ok(names)
}

fn make_parameter(
    contract: &Value,
    case_id: &str,
    location: &str,
    name: &str,
    parameter: &Value,
) -> Result<Parameter, String> {
    if location == "header"
        && ["authorization", "proxy-authorization", "cookie"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err("authentication headers require a security-scheme environment binding".into());
    }
    let required = parameter.get("required").map_or(Ok(false), |value| {
        value.as_bool().ok_or("parameter required must be boolean")
    })?;
    if location == "path" && !required {
        return Err("path parameters must be required".into());
    }
    let style = parameter
        .get("style")
        .map_or(
            Some(if location == "query" || location == "cookie" {
                "form"
            } else {
                "simple"
            }),
            Value::as_str,
        )
        .ok_or("parameter style must be a string")?;
    let explode = parameter
        .get("explode")
        .map_or(Ok(style == "form"), |value| {
            value.as_bool().ok_or("parameter explode must be boolean")
        })?;
    if parameter
        .get("allowReserved")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err("allowReserved parameter serialization is unsupported".into());
    }
    let parameter_schema = parameter
        .get("schema")
        .ok_or("parameter requires a supported schema; content parameters are unsupported")?;
    if parameter.get("content").is_some() {
        return Err("parameter schema and content cannot both be specified".into());
    }
    schema::compile(contract, parameter_schema)
        .map_err(|_| "parameter schema is invalid or unsupported")?;
    let resolved = schema::resolve(contract, parameter_schema)
        .map_err(|_| "parameter schema is unresolved or external")?;
    let kind = schema_type(&resolved)
        .ok_or("parameter schema requires a supported scalar or array type")?;
    let supported = match (location, style, kind) {
        ("path" | "header", "simple", "string" | "integer" | "number" | "boolean") => true,
        ("query", "form", "string" | "integer" | "number" | "boolean") => true,
        ("query", "form", "array") => {
            let item = resolved
                .get("items")
                .ok_or("query arrays require an items schema")?;
            let item =
                schema::resolve(contract, item).map_err(|_| "query array items are unresolved")?;
            matches!(
                schema_type(&item),
                Some("string" | "integer" | "number" | "boolean")
            )
        }
        _ => false,
    };
    if !supported {
        return Err("parameter location, type or serialization is unsupported".into());
    }
    let fixture = (location == "path").then(|| format!("{case_id}.path.{name}"));
    let value = if fixture.is_none() && required {
        let mut candidate = synthesize(contract, parameter_schema).ok_or(
            "cannot synthesize a supported required parameter; provide a simpler contract schema",
        )?;
        if location == "query" && candidate.as_array().is_some_and(Vec::is_empty) {
            let item = resolved
                .get("items")
                .and_then(|item| synthesize(contract, item))
                .ok_or("cannot synthesize a nonempty required query array")?;
            candidate = json!([item]);
            if !schema::compile(contract, parameter_schema)
                .is_ok_and(|validator| validator.is_valid(&candidate))
            {
                return Err(
                    "empty-only required query arrays have unsupported serialization".into(),
                );
            }
        }
        Some(candidate)
    } else {
        None
    };
    Ok(Parameter {
        name: name.to_owned(),
        location: location.to_owned(),
        required,
        style: style.to_owned(),
        explode,
        schema: parameter_schema.clone(),
        value,
        fixture,
    })
}

fn validate_security(contract: &Value, case: &mut Case) {
    let schemes = contract
        .pointer("/components/securitySchemes")
        .and_then(Value::as_object);
    let mut any_supported = case.security.is_empty();
    let mut undefined = false;
    for alternative in &case.security {
        let mut supported = true;
        for name in alternative.keys() {
            let Some(scheme) = schemes.and_then(|schemes| schemes.get(name)) else {
                undefined = true;
                supported = false;
                continue;
            };
            let Ok(scheme) = schema::resolve(contract, scheme) else {
                supported = false;
                continue;
            };
            let kind = scheme.get("type").and_then(Value::as_str);
            supported &= match kind {
                Some("http") => scheme
                    .get("scheme")
                    .and_then(Value::as_str)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer")),
                Some("apiKey") => {
                    scheme.get("in").and_then(Value::as_str) == Some("header")
                        && scheme
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| {
                                !name.is_empty() && !name.chars().any(char::is_control)
                            })
                }
                _ => false,
            };
        }
        any_supported |= supported;
    }
    if undefined {
        block(case, "security requirement references an undefined scheme");
    }
    if !any_supported {
        block(
            case,
            "security requires unsupported authentication; MVP supports bearer and header API keys",
        );
    }
}

fn validate_responses(contract: &Value, case: &mut Case) {
    let Some(responses) = case.responses.as_object() else {
        block(case, "operation responses must be an object");
        return;
    };
    let responses = responses.clone();
    let mut success = false;
    for (code, response) in responses {
        if code.starts_with("x-") {
            continue;
        }
        let valid_code = code == "default"
            || (code.len() == 3
                && matches!(code.as_bytes()[0], b'1'..=b'5')
                && (code.as_bytes()[1..].iter().all(u8::is_ascii_digit) || &code[1..] == "XX"));
        if !valid_code {
            block(case, "response status key is invalid");
            continue;
        }
        success |= code.starts_with('2');
        let response = match schema::resolve(contract, &response) {
            Ok(response) => response,
            Err(_) => {
                block(case, "response reference is unresolved or external");
                continue;
            }
        };
        if !response.get("description").is_some_and(Value::is_string) {
            block(case, "response requires a description");
        }
        if let Some(content) = response.get("content") {
            let Some(content) = content.as_object() else {
                block(case, "response content must be an object");
                continue;
            };
            for (media_type, media) in content {
                if !media.is_object() {
                    block(case, "response media type definition must be an object");
                    continue;
                }
                if media_type != "application/json"
                    && !(media_type.starts_with("application/") && media_type.ends_with("+json"))
                {
                    block(
                        case,
                        "response media type is unsupported; MVP supports JSON and bodyless responses",
                    );
                }
                if let Some(response_schema) = media.get("schema")
                    && schema::compile(contract, response_schema).is_err()
                {
                    block(case, "response schema is invalid or unsupported");
                }
            }
        }
        if let Some(headers) = response.get("headers") {
            let Some(headers) = headers.as_object() else {
                block(case, "response headers must be an object");
                continue;
            };
            for (name, header) in headers {
                // OpenAPI defines Content-Type through the response content map.
                if name.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                    block(case, "response header name is invalid");
                }
                match schema::resolve(contract, header) {
                    Ok(header) => {
                        if !header.is_object()
                            || header
                                .get("required")
                                .is_some_and(|value| !value.is_boolean())
                            || header
                                .get("explode")
                                .is_some_and(|value| !value.is_boolean())
                            || header
                                .get("style")
                                .is_some_and(|value| value.as_str() != Some("simple"))
                            || header.get("content").is_some()
                        {
                            block(case, "response header definition is invalid or unsupported");
                        }
                        if let Some(header_schema) = header.get("schema") {
                            if schema::compile(contract, header_schema).is_err() {
                                block(case, "response header schema is invalid or unsupported");
                            }
                            let scalar = schema::resolve(contract, header_schema).ok().is_some_and(
                                |schema| {
                                    matches!(
                                        schema_type(&schema),
                                        Some("string" | "integer" | "number" | "boolean")
                                    )
                                },
                            );
                            if !scalar {
                                block(
                                    case,
                                    "response header serialization supports scalar schemas only",
                                );
                            }
                        } else {
                            block(case, "response header requires a supported schema");
                        }
                    }
                    Err(_) => block(case, "response header reference is unresolved or external"),
                }
            }
        }
    }
    if !success {
        block(
            case,
            "positive case requires an explicitly documented 2xx response",
        );
    }
}

fn schema_type(schema: &Value) -> Option<&str> {
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        return Some(kind);
    }
    if schema.get("properties").is_some() {
        return Some("object");
    }
    schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .map(|value| match value {
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Null => "null",
        })
}

fn synthesize(contract: &Value, schema: &Value) -> Option<Value> {
    let validator = schema::compile(contract, schema).ok()?;
    let mut budget = 1024;
    synthesize_inner(contract, schema, 0, &mut budget)
        .filter(|candidate| validator.is_valid(candidate))
}

fn synthesize_inner(
    contract: &Value,
    schema: &Value,
    depth: usize,
    budget: &mut usize,
) -> Option<Value> {
    if depth > 8 || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let schema = schema::resolve(contract, schema).ok()?;
    if schema == Value::Bool(true) {
        return Some(Value::Null);
    }
    if let Some(value) = schema.get("const") {
        consume_literal_budget(value, budget)?;
        return Some(value.clone());
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let value = values.iter().find(|value| {
            schema::compile(contract, &schema).is_ok_and(|validator| validator.is_valid(value))
        })?;
        consume_literal_budget(value, budget)?;
        return Some(value.clone());
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let validator = schema::compile(contract, &schema).ok()?;
            return branches
                .iter()
                .filter_map(|branch| synthesize_inner(contract, branch, depth + 1, budget))
                .find(|candidate| validator.is_valid(candidate));
        }
    }
    match schema_type(&schema)? {
        "null" => Some(Value::Null),
        "boolean" => Some(Value::Bool(false)),
        "string" => {
            let minimum = schema.get("minLength").and_then(Value::as_u64).unwrap_or(1);
            if minimum > 128 {
                return None;
            }
            let maximum = schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .unwrap_or(128);
            let length = minimum.max(1).min(maximum);
            let default = "x".repeat(length as usize);
            let format = schema.get("format").and_then(Value::as_str);
            let candidate = match format {
                Some("uuid") => "00000000-0000-4000-8000-000000000001".to_owned(),
                Some("date") => "2000-01-01".to_owned(),
                Some("date-time") => "2000-01-01T00:00:00Z".to_owned(),
                Some("email") => "synthetic@example.test".to_owned(),
                Some("uri") => "https://example.test/synthetic".to_owned(),
                _ => default,
            };
            Some(Value::String(candidate))
        }
        "integer" | "number" => {
            let integer = schema_type(&schema) == Some("integer");
            let mut value = schema
                .get("minimum")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0);
            if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
                value = value.max(minimum + 1.0);
            }
            if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
                value = value.min(maximum);
            }
            if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
                value = value.min(maximum - 1.0);
            }
            if integer {
                value = value.ceil();
            }
            if let Some(multiple) = schema.get("multipleOf").and_then(Value::as_f64) {
                if multiple <= 0.0 {
                    return None;
                }
                value = (value / multiple).ceil() * multiple;
            }
            if integer && value >= i64::MIN as f64 && value < i64::MAX as f64 {
                Some(json!(value as i64))
            } else {
                serde_json::Number::from_f64(value).map(Value::Number)
            }
        }
        "array" => {
            let count = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            if count > 16 {
                return None;
            }
            let items = schema.get("items")?;
            let item = synthesize_inner(contract, items, depth + 1, budget)?;
            let mut values = vec![item; count as usize];
            if count > 1 && schema.get("uniqueItems") == Some(&Value::Bool(true)) {
                let resolved = schema::resolve(contract, items).ok()?;
                let choices = resolved.get("enum").and_then(Value::as_array)?;
                values = choices.iter().take(count as usize).cloned().collect();
                if values.len() != count as usize {
                    return None;
                }
            }
            Some(Value::Array(values))
        }
        "object" => {
            let mut values = Map::new();
            let properties = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required {
                    let name = name.as_str()?;
                    let property = properties?.get(name)?;
                    if property.get("readOnly") == Some(&Value::Bool(true)) {
                        continue;
                    }
                    values.insert(
                        name.to_owned(),
                        synthesize_inner(contract, property, depth + 1, budget)?,
                    );
                }
            }
            Some(Value::Object(values))
        }
        _ => None,
    }
}

fn consume_literal_budget(value: &Value, budget: &mut usize) -> Option<()> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    match value {
        Value::Array(values) => {
            for value in values {
                consume_literal_budget(value, budget)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                consume_literal_budget(value, budget)?;
            }
        }
        Value::String(value) if value.len() > 4096 => return None,
        _ => (),
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source {
            sha256: "0".repeat(64),
            openapi_version: "3.1.0".into(),
        }
    }

    fn contract(paths: Value) -> Value {
        json!({"openapi":"3.1.0", "info":{"title":"Synthetic", "version":"1"}, "paths":paths})
    }

    #[test]
    fn generation_is_deterministic_and_keeps_writes_blocked() {
        let contract = contract(json!({"/widgets/{id}": {
            "parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string"}}],
            "get":{"operationId":"getWidget","responses":{"200":{"description":"OK"}}},
            "delete":{"operationId":"deleteWidget","responses":{"204":{"description":"Deleted"}}}
        }}));
        let first = from_contract(contract.clone(), source()).unwrap();
        let second = from_contract(contract, source()).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(first.cases.len(), 2);
        assert!(!first.cases[0].blockers.is_empty());
        assert_eq!(
            first.cases[1].parameters[0].fixture.as_deref(),
            Some("getWidget.path.id")
        );
        assert!(first.cases[1].blockers.is_empty());
    }

    #[test]
    fn duplicate_operations_and_external_path_refs_are_rejected() {
        let duplicate = contract(
            json!({"/a":{"get":{"operationId":"duplicate","responses":{"200":{"description":"OK"}}}},"/b":{"get":{"operationId":"duplicate","responses":{"200":{"description":"OK"}}}}}),
        );
        assert!(
            from_contract(duplicate, source())
                .unwrap_err()
                .contains("unique")
        );
        let external = contract(json!({"/a":{"$ref":"https://example.test/path.yaml"}}));
        assert!(from_contract(external, source()).is_err());
    }

    #[test]
    fn path_item_reference_policy_applies_at_every_hop() {
        let mut document = contract(json!({"/widgets":{"$ref":"#/components/pathItems/Base"}}));
        document["components"] = json!({"pathItems":{
            "Base":{"$ref":"#/components/pathItems/Parent","get":{"operationId":"read","responses":{"200":{"description":"OK"}}}},
            "Parent":{"post":{"operationId":"create","responses":{"201":{"description":"Created"}}}}
        }});
        assert!(from_contract(document.clone(), source()).is_err());
        document["components"]["pathItems"]["Base"]
            .as_object_mut()
            .unwrap()
            .remove("get");
        let suite = from_contract(document, source()).unwrap();
        assert_eq!(suite.cases.len(), 1);
        assert_eq!(suite.cases[0].id, "create");
        assert!(!suite.cases[0].blockers.is_empty());
    }

    #[test]
    fn required_query_arrays_have_a_serializable_nonempty_candidate() {
        let mut document = contract(json!({"/widgets":{"get":{
            "parameters":[{"name":"tags","in":"query","required":true,"schema":{"type":"array","items":{"type":"string","enum":["synthetic"]}}}],
            "responses":{"200":{"description":"OK"}}
        }}}));
        let suite = from_contract(document.clone(), source()).unwrap();
        assert_eq!(
            suite.cases[0].parameters[0].value,
            Some(json!(["synthetic"]))
        );
        assert!(suite.cases[0].blockers.is_empty());
        document["paths"]["/widgets"]["get"]["parameters"][0]["schema"]["maxItems"] = json!(0);
        assert!(
            !from_contract(document, source()).unwrap().cases[0]
                .blockers
                .is_empty()
        );
    }

    #[test]
    fn required_query_arrays_are_schema_valid_and_refs_stay_inert() {
        let contract = contract(json!({"/widgets":{"get":{
            "parameters":[{"name":"tags","in":"query","required":true,"schema":{"type":"array","minItems":2,"items":{"type":"string","enum":["a","b"]}}}],
            "responses":{"200":{"$ref":"https://example.test/response.yaml"}}
        }}}));
        let suite = from_contract(contract, source()).unwrap();
        assert_eq!(suite.cases[0].parameters[0].value, Some(json!(["a", "a"])));
        assert!(
            suite.cases[0]
                .blockers
                .iter()
                .any(|reason| reason.contains("external"))
        );
    }
}
