use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};

use jsonschema::Validator;
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::{Case, Suite};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct RunOptions {
    pub suite: PathBuf,
    pub max_input_bytes: u64,
    pub base_url: Option<Url>,
    pub execute: bool,
    pub operations: Vec<String>,
    pub fixtures: Vec<(String, String)>,
    pub auth_env: Vec<(String, String)>,
    pub timeout_ms: u32,
    pub rate_per_second: u16,
    pub max_requests: u32,
}

struct Prepared {
    request: reqwest::Request,
    responses: BTreeMap<String, ResponsePlan>,
}

struct ResponsePlan {
    content: Vec<MediaPlan>,
    headers: Vec<HeaderPlan>,
}

struct MediaPlan {
    media_type: String,
    validator: Option<Validator>,
}

struct HeaderPlan {
    name: HeaderName,
    required: bool,
    validator: Option<Validator>,
}

pub async fn run(options: RunOptions) -> Result<Value, String> {
    if !(1..=crate::strict_json::MAX_CONFIGURED_BYTES).contains(&options.max_input_bytes) {
        return Err("suite input limit must be between 1 byte and 64 MiB".into());
    }
    if options.timeout_ms == 0 || options.rate_per_second == 0 || options.max_requests == 0 {
        return Err("timeout, rate and request limit must be positive".into());
    }
    let mut bytes = Vec::new();
    File::open(&options.suite)
        .map_err(|_| "cannot open suite file")?
        .take(options.max_input_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read suite file")?;
    if bytes.len() as u64 > options.max_input_bytes {
        return Err("suite exceeds the configured input limit".into());
    }
    let raw = crate::strict_json::parse_json_with_limit(&bytes, options.max_input_bytes as usize)?;
    let suite: Suite = serde_json::from_value(raw).map_err(|_| "invalid suite structure")?;
    validate_suite(&suite)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let contract_digest = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&suite.contract).map_err(|_| "cannot hash frozen contract")?
        )
    );
    let fixtures = unique_bindings(&options.fixtures, "fixture")?;
    let auth_env = unique_bindings(&options.auth_env, "authentication")?;
    if let Some(base) = &options.base_url {
        validate_base(base)?;
    }
    let selected = selection(&suite, &options.operations)?;
    if options.execute && options.base_url.is_none() {
        return Err("execution requires an explicit --base-url".into());
    }
    if options.execute && options.operations.is_empty() {
        return Err("execution requires at least one --operation".into());
    }
    if options.execute && selected.len() > options.max_requests as usize {
        return Err("selected operations exceed --max-requests".into());
    }
    let client = Client::builder()
        .http1_only()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_millis(u64::from(options.timeout_ms)))
        .build()
        .map_err(|_| "cannot initialize HTTP client")?;

    // Complete preflight before the first request, including every selected case.
    let mut rows = Vec::new();
    let mut prepared = BTreeMap::new();
    let mut blocked = 0;
    for (index, case) in suite.cases.iter().enumerate() {
        let is_selected = selected.contains(&index);
        if !is_selected {
            rows.push(case_row(case, "skipped", &case.blockers));
            continue;
        }
        match prepare(case, &suite, &options, &client, &fixtures, &auth_env) {
            Ok(plan) => {
                if let Some(plan) = plan {
                    prepared.insert(index, plan);
                }
                rows.push(case_row(case, "ready", &[]));
            }
            Err(reasons) => {
                blocked += 1;
                rows.push(case_row(case, "blocked", &reasons));
            }
        }
    }
    if options.execute && blocked != 0 {
        return Err(format!(
            "preflight blocked {blocked} selected operation(s); no requests sent (use preview for reasons)"
        ));
    }
    let mut passed = 0;
    let mut failed = 0;
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.rate_per_second));
    let mut previous_started = None;
    if options.execute {
        for (index, plan) in prepared {
            if let Some(started) = previous_started {
                let elapsed = Instant::now().duration_since(started);
                if elapsed < interval {
                    tokio::time::sleep(interval - elapsed).await;
                }
            }
            previous_started = Some(Instant::now());
            let row = execute(&client, &suite.cases[index], plan).await;
            if row["state"] == "passed" {
                passed += 1;
            } else {
                failed += 1;
            }
            rows[index] = row;
        }
    }
    let count = suite.cases.len();
    let ready = selected.len() - blocked;
    Ok(json!({
        "schema_version": 1,
        "kind": "suite",
        "mode": if options.execute { "execute" } else { "preview" },
        "executed": options.execute,
        "passed": options.execute.then_some(failed == 0 && passed > 0),
        "ready": blocked == 0 && !selected.is_empty(),
        "suite_digest": digest,
        "contract_digest": contract_digest,
        "source_digest": suite.source.sha256,
        "runner": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "git_revision": if env!("RUSTA_GIT_REVISION").is_empty() { None } else { Some(env!("RUSTA_GIT_REVISION")) },
            "source_dirty": match env!("RUSTA_SOURCE_DIRTY") {
                "true" => Some(true), "false" => Some(false), _ => None,
            },
            "target": env!("RUSTA_BUILD_TARGET"),
            "rustc_version": env!("RUSTA_RUSTC_VERSION"),
            "build_profile": env!("RUSTA_BUILD_PROFILE"),
        },
        "execution": {
            "requests_per_operation": 1,
            "concurrency": 1,
            "rate_per_second": options.rate_per_second,
            "max_requests": options.max_requests,
            "timeout_ms": options.timeout_ms,
            "response_limit_bytes": MAX_RESPONSE_BYTES,
            "redirects": false,
            "retries": false,
            "environment_proxies": false,
        },
        "counts": {
            "total": count, "selected": selected.len(), "passed": passed,
            "failed": failed, "blocked": blocked, "skipped": count - selected.len(),
            "ready": if options.execute { 0 } else { ready },
        },
        "cases": rows,
    }))
}

fn validate_suite(suite: &Suite) -> Result<(), String> {
    if suite.suite_version != 1 {
        return Err("unsupported suite version".into());
    }
    if suite.source.sha256.len() != 64
        || !suite
            .source
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("invalid source digest".into());
    }
    let generated = crate::openapi::from_contract(suite.contract.clone(), suite.source.clone())?;
    if suite.cases.len() != generated.cases.len() {
        return Err(
            "suite must retain every operation; use --operation to select execution".into(),
        );
    }
    let mut ids = BTreeSet::new();
    for case in &suite.cases {
        if !ids.insert(&case.id) {
            return Err("duplicate case identifier".into());
        }
        let original = generated
            .cases
            .iter()
            .find(|item| item.id == case.id)
            .ok_or("suite contains an operation absent from its contract")?;
        if case.method != original.method
            || case.path != original.path
            || case.security != original.security
            || case.responses != original.responses
            || case.parameters.len() != original.parameters.len()
            || case.blockers != original.blockers
        {
            return Err(
                "suite operation assertions differ from the frozen contract; regenerate the suite"
                    .into(),
            );
        }
        let mut parameters = BTreeSet::new();
        for parameter in &case.parameters {
            if !parameters.insert((&parameter.location, &parameter.name)) {
                return Err("duplicate suite parameter".into());
            }
            let expected = original
                .parameters
                .iter()
                .find(|item| item.name == parameter.name && item.location == parameter.location)
                .ok_or("suite parameter absent from frozen contract")?;
            if parameter.required != expected.required
                || parameter.style != expected.style
                || parameter.explode != expected.explode
                || parameter.schema != expected.schema
            {
                return Err("suite parameter assertions differ from the frozen contract".into());
            }
        }
    }
    Ok(())
}

fn unique_bindings<'a>(
    pairs: &'a [(String, String)],
    kind: &str,
) -> Result<BTreeMap<&'a str, &'a str>, String> {
    let mut result = BTreeMap::new();
    for (key, value) in pairs {
        if key.is_empty()
            || value.is_empty()
            || result.insert(key.as_str(), value.as_str()).is_some()
        {
            return Err(format!("invalid or duplicate {kind} binding"));
        }
    }
    Ok(result)
}

fn selection(suite: &Suite, operations: &[String]) -> Result<BTreeSet<usize>, String> {
    if operations.is_empty() {
        return Ok((0..suite.cases.len()).collect());
    }
    let mut result = BTreeSet::new();
    for name in operations {
        let index = suite
            .cases
            .iter()
            .position(|case| &case.id == name)
            .ok_or("selected operation is absent from suite")?;
        if !result.insert(index) {
            return Err("operation selected more than once".into());
        }
    }
    Ok(result)
}

fn validate_base(base: &Url) -> Result<(), String> {
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("base URL must be HTTP(S), with no credentials, query or fragment".into());
    }
    Ok(())
}

fn prepare(
    case: &Case,
    suite: &Suite,
    options: &RunOptions,
    client: &Client,
    fixtures: &BTreeMap<&str, &str>,
    auth_env: &BTreeMap<&str, &str>,
) -> Result<Option<Prepared>, Vec<String>> {
    let mut reasons = case.blockers.clone();
    if !matches!(case.method.as_str(), "GET" | "HEAD") {
        reasons.push("state_changing_execution_unsupported".into());
    }
    if case.body.is_some() {
        reasons.push("request_body_execution_unsupported".into());
    }
    let mut values = BTreeMap::new();
    for parameter in &case.parameters {
        let validator = match crate::schema::compile(&suite.contract, &parameter.schema) {
            Ok(validator) => validator,
            Err(_) => {
                reasons.push("parameter_schema_unsupported".into());
                continue;
            }
        };
        let value = if let Some(slot) = &parameter.fixture {
            if let Some(raw) = fixtures.get(slot.as_str()) {
                Some(parse_parameter(raw, &validator))
            } else {
                reasons.push(format!("fixture_required:{slot}"));
                None
            }
        } else {
            parameter.value.clone()
        };
        if let Some(value) = value {
            if !validator.is_valid(&value) {
                reasons.push(format!("fixture_schema_mismatch:{}", parameter.name));
            }
            values.insert(
                (parameter.location.as_str(), parameter.name.as_str()),
                value,
            );
        } else if parameter.required && parameter.fixture.is_none() {
            reasons.push(format!("parameter_value_required:{}", parameter.name));
        }
    }
    let responses = match prepare_responses(&suite.contract, &case.responses) {
        Ok(plans) => Some(plans),
        Err(reason) => {
            reasons.push(reason);
            None
        }
    };
    let auth = match authentication(suite, case, auth_env, options.execute) {
        Ok(headers) => Some(headers),
        Err(reason) => {
            reasons.push(reason);
            None
        }
    };
    if options.base_url.is_none() {
        reasons.push("base_url_required".into());
    }
    let mut request = None;
    if let Some(base) = &options.base_url {
        match build_request(client, base, case, &values, auth.unwrap_or_default()) {
            Ok(built) => request = Some(built),
            Err(reason) => reasons.push(reason),
        }
    }
    reasons.sort();
    reasons.dedup();
    if !reasons.is_empty() {
        return Err(reasons);
    }
    if options.execute {
        Ok(Some(Prepared {
            request: request.expect("base URL checked"),
            responses: responses.expect("response schemas checked"),
        }))
    } else {
        Ok(None)
    }
}

fn parse_parameter(raw: &str, validator: &Validator) -> Value {
    let string = Value::String(raw.to_owned());
    if validator.is_valid(&string) {
        return string;
    }
    serde_json::from_str(raw).unwrap_or(string)
}

fn authentication(
    suite: &Suite,
    case: &Case,
    bindings: &BTreeMap<&str, &str>,
    execute: bool,
) -> Result<HeaderMap, String> {
    if case.security.is_empty() {
        return Ok(HeaderMap::new());
    }
    for alternative in &case.security {
        if let Ok(headers) = authentication_alternative(suite, alternative, bindings, execute) {
            return Ok(headers);
        }
    }
    Err("authentication_binding_required_or_unsupported".into())
}

fn authentication_alternative(
    suite: &Suite,
    alternative: &BTreeMap<String, Vec<String>>,
    bindings: &BTreeMap<&str, &str>,
    execute: bool,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for name in alternative.keys() {
        let Some(definition) = suite
            .contract
            .pointer("/components/securitySchemes")
            .and_then(|schemes| schemes.get(name))
        else {
            return Err("security_scheme_unresolved".into());
        };
        let definition = crate::schema::resolve(&suite.contract, definition)
            .map_err(|_| "security_scheme_unresolved")?;
        let Some(variable) = bindings.get(name.as_str()) else {
            return Err("authentication_binding_required".into());
        };
        let header = match (
            definition["type"].as_str(),
            definition["scheme"].as_str(),
            definition["in"].as_str(),
        ) {
            (Some("http"), Some(scheme), _) if scheme.eq_ignore_ascii_case("bearer") => {
                "authorization"
            }
            (Some("apiKey"), _, Some("header")) => definition["name"].as_str().unwrap_or(""),
            _ => return Err("security_scheme_unsupported".into()),
        };
        let header =
            HeaderName::from_bytes(header.as_bytes()).map_err(|_| "auth_header_invalid")?;
        if matches!(
            header.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "proxy-authorization"
        ) {
            return Err("auth_header_unsupported".into());
        }
        if headers.contains_key(&header) {
            return Err("conflicting_authentication_headers".into());
        }
        // Preview checks binding names only. Secret values are loaded exclusively for execution.
        if execute {
            let secret =
                std::env::var(variable).map_err(|_| "authentication_environment_missing")?;
            if secret.is_empty() {
                return Err("authentication_environment_empty".into());
            }
            let value = if definition["type"] == "http" {
                format!("Bearer {secret}")
            } else {
                secret
            };
            let value =
                HeaderValue::from_str(&value).map_err(|_| "authentication_value_invalid")?;
            if headers.insert(header, value).is_some() {
                return Err("conflicting_authentication_headers".into());
            }
        } else {
            headers.insert(header, HeaderValue::from_static("preview"));
        }
    }
    Ok(headers)
}

fn scalar(value: &Value) -> Result<String, String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(value.to_string()),
        _ => Err("parameter_serialization_unsupported".into()),
    }
}

fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to string");
        }
    }
    encoded
}

fn build_request(
    client: &Client,
    base: &Url,
    case: &Case,
    values: &BTreeMap<(&str, &str), Value>,
    mut headers: HeaderMap,
) -> Result<reqwest::Request, String> {
    if !case.path.starts_with('/') || case.path.contains(['?', '#', '\\', '%']) {
        return Err("operation_path_unsupported".into());
    }
    let mut url = base.clone();
    if case
        .parameters
        .iter()
        .any(|parameter| parameter.location == "path" && parameter.style != "simple")
    {
        return Err("parameter_serialization_unsupported".into());
    }
    if url.cannot_be_a_base() {
        return Err("base_url_path_unsupported".into());
    }
    let mut path = url
        .path()
        .strip_suffix('/')
        .unwrap_or(url.path())
        .to_owned();
    for part in case.path[1..].split('/') {
        let segment = render_segment(part, values)?;
        if matches!(segment.as_str(), "." | "..") {
            return Err("operation_path_unsupported".into());
        }
        path.push('/');
        path.push_str(&segment);
    }
    // The rendered fixtures and base path are already encoded. The full-path
    // setter preserves those escapes; a path-segment setter would encode '%' again.
    url.set_path(&path);
    let mut query = Vec::new();
    for parameter in &case.parameters {
        let Some(value) = values.get(&(parameter.location.as_str(), parameter.name.as_str()))
        else {
            continue;
        };
        match parameter.location.as_str() {
            "path" => {}
            "query" if parameter.style == "form" => {
                let name = encode(&parameter.name);
                if let Some(items) = value.as_array() {
                    if parameter.required && items.is_empty() {
                        return Err("required_empty_query_array_unsupported".into());
                    }
                    let items = items.iter().map(scalar).collect::<Result<Vec<_>, _>>()?;
                    if parameter.explode {
                        query.extend(
                            items
                                .iter()
                                .map(|value| format!("{name}={}", encode(value))),
                        );
                    } else {
                        query.push(format!(
                            "{name}={}",
                            items
                                .iter()
                                .map(|value| encode(value))
                                .collect::<Vec<_>>()
                                .join(",")
                        ));
                    }
                } else {
                    query.push(format!("{name}={}", encode(&scalar(value)?)));
                }
            }
            "header" if parameter.style == "simple" => {
                let name = HeaderName::from_bytes(parameter.name.as_bytes())
                    .map_err(|_| "parameter_header_invalid")?;
                if matches!(
                    name.as_str(),
                    "authorization"
                        | "proxy-authorization"
                        | "cookie"
                        | "set-cookie"
                        | "host"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                ) {
                    return Err("parameter_header_requires_runtime_auth_or_is_reserved".into());
                }
                let value = HeaderValue::from_str(&scalar(value)?)
                    .map_err(|_| "parameter_header_invalid")?;
                if headers.insert(name, value).is_some() {
                    return Err("conflicting_request_headers".into());
                }
            }
            _ => return Err("parameter_serialization_unsupported".into()),
        }
    }
    if !query.is_empty() {
        url.set_query(Some(&query.join("&")));
    }
    client
        .request(
            Method::from_bytes(case.method.as_bytes()).map_err(|_| "method_invalid")?,
            url,
        )
        .headers(headers)
        .build()
        .map_err(|_| "request_configuration_invalid".into())
}

/// Interpret contract tokens once, encoding fixture data for simple expansion.
fn render_segment(
    template: &str,
    values: &BTreeMap<(&str, &str), Value>,
) -> Result<String, String> {
    let mut rendered = String::new();
    let mut remainder = template;
    while let Some(start) = remainder.find('{') {
        let literal = &remainder[..start];
        if literal.contains('}') {
            return Err("operation_path_unsupported".into());
        }
        rendered.push_str(literal);
        let token = &remainder[start + 1..];
        let end = token.find('}').ok_or("operation_path_unsupported")?;
        let name = &token[..end];
        if name.is_empty() || name.contains('{') {
            return Err("operation_path_unsupported".into());
        }
        let value = values.get(&("path", name)).ok_or("path_fixture_required")?;
        rendered.push_str(&encode(&scalar(value)?));
        remainder = &token[end + 1..];
    }
    if remainder.contains('}') {
        return Err("operation_path_unsupported".into());
    }
    rendered.push_str(remainder);
    Ok(rendered)
}

fn prepare_responses(
    contract: &Value,
    responses: &Value,
) -> Result<BTreeMap<String, ResponsePlan>, String> {
    let responses = responses.as_object().ok_or("response_map_invalid")?;
    if !responses.keys().any(|key| {
        key == "2XX"
            || key
                .parse::<u16>()
                .is_ok_and(|code| (200..300).contains(&code))
    }) {
        return Err("documented_success_response_required".into());
    }
    let mut plans = BTreeMap::new();
    for (code, definition) in responses {
        if code.starts_with("x-") {
            continue;
        }
        let response = crate::schema::resolve(contract, definition)
            .map_err(|_| "response_reference_unsupported")?;
        let mut content = Vec::new();
        if let Some(types) = response.get("content") {
            let types = types.as_object().ok_or("response_content_invalid")?;
            for (media_type, entry) in types {
                if !entry.is_object() {
                    return Err("response_media_definition_invalid".into());
                }
                let media_type = media_type.to_ascii_lowercase();
                if !(is_json(&media_type)
                    || matches!(media_type.as_str(), "text/plain" | "*/*" | "application/*"))
                {
                    return Err("response_media_type_unsupported".into());
                }
                let validator = entry
                    .get("schema")
                    .map(|schema| {
                        crate::schema::compile(contract, schema)
                            .map_err(|_| "response_schema_unsupported".to_owned())
                    })
                    .transpose()?;
                content.push(MediaPlan {
                    media_type,
                    validator,
                });
            }
        }
        let mut headers = Vec::new();
        if let Some(definitions) = response.get("headers") {
            for (name, definition) in definitions.as_object().ok_or("response_headers_invalid")? {
                if name.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| "response_header_invalid")?;
                let definition = crate::schema::resolve(contract, definition)
                    .map_err(|_| "response_header_reference_unsupported")?;
                if !definition.is_object() {
                    return Err("response_header_definition_invalid".into());
                }
                if definition
                    .get("required")
                    .is_some_and(|value| !value.is_boolean())
                    || definition
                        .get("explode")
                        .is_some_and(|value| !value.is_boolean())
                    || definition
                        .get("style")
                        .is_some_and(|value| value.as_str() != Some("simple"))
                {
                    return Err("response_header_serialization_unsupported".into());
                }
                if definition.get("content").is_some() {
                    return Err("response_header_content_unsupported".into());
                }
                let validator = definition
                    .get("schema")
                    .map(|schema| {
                        if !header_schema_supported(contract, schema, 0) {
                            return Err("response_header_serialization_unsupported".to_owned());
                        }
                        crate::schema::compile(contract, schema)
                            .map_err(|_| "response_header_schema_unsupported".to_owned())
                    })
                    .transpose()?;
                headers.push(HeaderPlan {
                    name,
                    required: definition["required"].as_bool().unwrap_or(false),
                    validator,
                });
            }
        }
        plans.insert(code.clone(), ResponsePlan { content, headers });
    }
    Ok(plans)
}

fn is_json(media_type: &str) -> bool {
    media_type == "application/json"
        || media_type.starts_with("application/") && media_type.ends_with("+json")
}

fn header_schema_supported(contract: &Value, schema: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    if schema.is_boolean() {
        return true;
    }
    let Some(object) = schema.as_object() else {
        return false;
    };
    let composite = |value: &Value| matches!(value.as_str(), Some("array" | "object"));
    if object.get("type").is_some_and(|value| {
        composite(value)
            || value
                .as_array()
                .is_some_and(|values| values.iter().any(composite))
    }) || [
        "properties",
        "patternProperties",
        "items",
        "prefixItems",
        "contains",
        "additionalProperties",
        "unevaluatedProperties",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
    {
        return false;
    }
    if object
        .get("const")
        .is_some_and(|value| value.is_array() || value.is_object())
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.is_array() || value.is_object())
            })
    {
        return false;
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(children) = object.get(key) {
            let Some(children) = children.as_array() else {
                return false;
            };
            if !children
                .iter()
                .all(|child| header_schema_supported(contract, child, depth + 1))
            {
                return false;
            }
        }
    }
    if object.contains_key("$ref") {
        return crate::schema::resolve(contract, schema)
            .is_ok_and(|resolved| header_schema_supported(contract, &resolved, depth + 1));
    }
    true
}

fn matching_media<'a>(plans: &'a [MediaPlan], actual: &str) -> Option<&'a MediaPlan> {
    plans
        .iter()
        .find(|plan| plan.media_type == actual)
        .or_else(|| {
            plans.iter().find(|plan| {
                plan.media_type == "application/*+json"
                    && actual.starts_with("application/")
                    && actual.ends_with("+json")
            })
        })
        .or_else(|| {
            plans.iter().find(|plan| {
                plan.media_type == "application/*" && actual.starts_with("application/")
            })
        })
        .or_else(|| plans.iter().find(|plan| plan.media_type == "*/*"))
}

fn case_row(case: &Case, state: &str, reasons: &[String]) -> Value {
    json!({
        "id": case.id, "method": case.method, "state": state, "reasons": reasons,
        "status": null,
        "checks": {"status": "not_run", "media_type": "not_run", "json_syntax": "not_run", "body_schema": "not_run", "headers": "not_run"},
    })
}

async fn execute(client: &Client, case: &Case, plan: Prepared) -> Value {
    let mut row = case_row(case, "failed", &[]);
    let started = Instant::now();
    let mut response = match client.execute(plan.request).await {
        Ok(response) => response,
        Err(error) => {
            row["reasons"] = json!([if error.is_timeout() {
                "timeout"
            } else {
                "transport_error"
            }]);
            row["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
            return row;
        }
    };
    let status = response.status().as_u16();
    row["status"] = json!(status);
    let headers = response.headers().clone();
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES => {
                body.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => {
                row["reasons"] = json!(["response_size_limit_exceeded"]);
                row["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                return row;
            }
            Ok(None) => break,
            Err(error) => {
                row["reasons"] = json!([if error.is_timeout() {
                    "timeout"
                } else {
                    "response_body_error"
                }]);
                row["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
                return row;
            }
        }
    }
    row["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    let explicit_definition = plan
        .responses
        .get(&status.to_string())
        .or_else(|| plan.responses.get(&format!("{}XX", status / 100)));
    let definition = explicit_definition.or_else(|| plan.responses.get("default"));
    let mut reasons = Vec::new();
    if (200..300).contains(&status) && explicit_definition.is_some() {
        row["checks"]["status"] = json!("passed");
    } else {
        row["checks"]["status"] = json!("failed");
        reasons.push("unexpected_positive_status");
    }
    if let Some(definition) = definition {
        let headers_passed = definition.headers.iter().all(|header| {
            if headers.get_all(&header.name).iter().count() > 1 {
                return false;
            }
            match headers.get(&header.name) {
                None => !header.required,
                Some(value) => value.to_str().is_ok_and(|value| {
                    header.validator.as_ref().is_none_or(|validator| {
                        let parsed = parse_parameter(value, validator);
                        !parsed.is_object() && !parsed.is_array() && validator.is_valid(&parsed)
                    })
                }),
            }
        });
        row["checks"]["headers"] = json!(if definition.headers.is_empty() {
            "not_documented"
        } else if headers_passed {
            "passed"
        } else {
            "failed"
        });
        if !headers_passed {
            reasons.push("response_header_mismatch");
        }
        if case.method == "HEAD" || matches!(status, 204 | 205) {
            row["checks"]["media_type"] = json!("not_applicable");
            row["checks"]["json_syntax"] = json!("not_applicable");
            row["checks"]["body_schema"] = json!("not_applicable");
            if !body.is_empty() {
                reasons.push("unexpected_body_for_bodyless_response");
            }
        } else if definition.content.is_empty() {
            row["checks"]["media_type"] = json!("not_documented");
            row["checks"]["json_syntax"] = json!("not_documented");
            row["checks"]["body_schema"] = json!("not_documented");
        } else {
            let actual = headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                });
            match actual
                .as_deref()
                .and_then(|actual| matching_media(&definition.content, actual))
            {
                None => {
                    row["checks"]["media_type"] = json!("failed");
                    reasons.push("response_media_type_mismatch");
                }
                Some(media) => {
                    row["checks"]["media_type"] = json!("passed");
                    let json_body = if actual.as_deref().is_some_and(is_json) {
                        let parsed = crate::strict_json::parse_json(&body).ok();
                        row["checks"]["json_syntax"] =
                            json!(if parsed.is_some() { "passed" } else { "failed" });
                        if parsed.is_none() {
                            reasons.push("invalid_json_response");
                        }
                        parsed
                    } else {
                        row["checks"]["json_syntax"] = json!("not_applicable");
                        None
                    };
                    if let Some(validator) = &media.validator {
                        let value = if actual.as_deref().is_some_and(is_json) {
                            json_body
                        } else if actual.as_deref() == Some("text/plain") {
                            String::from_utf8(body).ok().map(Value::String)
                        } else {
                            None
                        };
                        let valid = value
                            .as_ref()
                            .is_some_and(|value| validator.is_valid(value));
                        row["checks"]["body_schema"] =
                            json!(if valid { "passed" } else { "failed" });
                        if !valid {
                            reasons.push("response_schema_mismatch");
                        }
                    } else {
                        row["checks"]["body_schema"] = json!("not_documented");
                    }
                }
            }
        }
    }
    row["state"] = json!(if reasons.is_empty() {
        "passed"
    } else {
        "failed"
    });
    row["reasons"] = json!(reasons);
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Parameter;

    fn case() -> Case {
        Case {
            id: "getWidget".into(),
            method: "GET".into(),
            path: "/widgets/{id}".into(),
            parameters: vec![Parameter {
                name: "id".into(),
                location: "path".into(),
                required: true,
                style: "simple".into(),
                explode: false,
                schema: json!({"type":"string"}),
                value: None,
                fixture: Some("getWidget.path.id".into()),
            }],
            security: vec![],
            responses: json!({"200":{"description":"OK"}}),
            body: None,
            blockers: vec![],
        }
    }

    #[test]
    fn query_encoding_keeps_delimiters_distinct_from_values() {
        assert_eq!(encode("red,blue /☃"), "red%2Cblue%20%2F%E2%98%83");
    }

    #[test]
    fn base_requires_explicit_clean_http_target() {
        for url in [
            "https://u:p@example.test/api",
            "https://example.test/api?q=x",
            "https://example.test/#x",
            "file:///tmp/x",
        ] {
            assert!(validate_base(&Url::parse(url).unwrap()).is_err());
        }
        assert!(validate_base(&Url::parse("https://example.test/api/v1").unwrap()).is_ok());
    }

    #[test]
    fn path_fixture_cannot_escape_the_target_prefix() {
        let client = Client::new();
        let base = Url::parse("https://example.test/api/v1").unwrap();
        let mut values = BTreeMap::new();
        values.insert(("path", "id"), json!("a/b?token#fragment"));
        let request = build_request(&client, &base, &case(), &values, HeaderMap::new()).unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://example.test/api/v1/widgets/a%2Fb%3Ftoken%23fragment"
        );
        for traversal in [".", ".."] {
            values.insert(("path", "id"), json!(traversal));
            assert!(build_request(&client, &base, &case(), &values, HeaderMap::new()).is_err());
        }
        let mut mixed = case();
        mixed.path = "/widgets/.{id}".into();
        values.insert(("path", "id"), json!("."));
        assert!(build_request(&client, &base, &mixed, &values, HeaderMap::new()).is_err());
    }

    #[test]
    fn simple_path_fixtures_encode_data_once_and_preserve_literal_paths() {
        let client = Client::new();
        let base = Url::parse("https://example.test/api/encoded%2Fprefix/").unwrap();
        let mut case = case();
        case.path = "/literal;part,part/prefix-{id}.json".into();
        for (fixture, encoded) in [
            ("a;b,c", "a%3Bb%2Cc"),
            (":@&=+$!'()*[]", "%3A%40%26%3D%2B%24%21%27%28%29%2A%5B%5D"),
            ("a/b?x#y", "a%2Fb%3Fx%23y"),
            ("%2F%2e%2e", "%252F%252e%252e"),
            ("\\evil.test", "%5Cevil.test"),
            ("☃/{id}", "%E2%98%83%2F%7Bid%7D"),
            ("AZaz09-._~", "AZaz09-._~"),
        ] {
            let values = BTreeMap::from([(("path", "id"), json!(fixture))]);
            let request = build_request(&client, &base, &case, &values, HeaderMap::new()).unwrap();
            assert_eq!(
                request.url().as_str(),
                format!(
                    "https://example.test/api/encoded%2Fprefix/literal;part,part/prefix-{encoded}.json"
                ),
                "fixture must remain simple-style parameter data: {fixture:?}"
            );
        }
    }

    #[test]
    fn exploded_and_joined_query_arrays_preserve_values() {
        let client = Client::new();
        let base = Url::parse("https://example.test/api").unwrap();
        let mut case = case();
        case.parameters.push(Parameter {
            name: "tags".into(),
            location: "query".into(),
            required: true,
            style: "form".into(),
            explode: true,
            schema: json!({"type":"array"}),
            value: None,
            fixture: None,
        });
        let mut values = BTreeMap::new();
        values.insert(("path", "id"), json!("w1"));
        values.insert(("query", "tags"), json!(["red,blue", "white space"]));
        let request = build_request(&client, &base, &case, &values, HeaderMap::new()).unwrap();
        assert_eq!(
            request.url().query(),
            Some("tags=red%2Cblue&tags=white%20space")
        );
        case.parameters[1].explode = false;
        let request = build_request(&client, &base, &case, &values, HeaderMap::new()).unwrap();
        assert_eq!(request.url().query(), Some("tags=red%2Cblue,white%20space"));
        values.insert(("query", "tags"), json!([]));
        assert!(build_request(&client, &base, &case, &values, HeaderMap::new()).is_err());
    }

    #[test]
    fn fixture_braces_remain_literal_when_they_name_another_parameter() {
        let client = Client::new();
        let base = Url::parse("https://example.test/api/v1").unwrap();
        let mut case = case();
        case.path = "/{a}/{b}".into();
        case.parameters[0].name = "a".into();
        let mut second = case.parameters[0].clone();
        second.name = "b".into();
        case.parameters.push(second);
        let values = BTreeMap::from([(("path", "a"), json!("{b}")), (("path", "b"), json!("ok"))]);
        let request = build_request(&client, &base, &case, &values, HeaderMap::new()).unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://example.test/api/v1/%7Bb%7D/ok"
        );
    }
}
