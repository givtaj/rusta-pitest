//! Immutable HTTP plans for explicitly selected workload requests.
//!
//! Secret material is loaded only during execution preparation. It has no
//! Debug/Serialize implementation and never appears in an outcome or error.
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path},
    time::Duration,
};

use jsonschema::Validator;
use reqwest::{
    Client, Method, Request, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 8192;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 8192;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSpec {
    pub method: String,
    pub path: String,
    pub body_env: Option<String>,
    pub body_file: Option<String>,
    pub content_type: Option<String>,
    pub idempotency_key_env: Option<String>,
    pub expect_status: Option<u16>,
    pub response_schema: Option<Value>,
}

pub struct PreparedRequest {
    request: Request,
    expect_status: Option<u16>,
    response_schema: Option<Validator>,
    executable: bool,
}

pub struct Outcome {
    pub passed: bool,
    pub status: Option<u16>,
    pub reason: &'static str,
    pub response_complete: bool,
}

pub fn client(timeout_ms: u64) -> Result<Client, String> {
    if timeout_ms == 0 {
        return Err("timeout_must_be_positive".into());
    }
    Client::builder()
        .http1_only()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|_| "http_client_configuration_invalid".into())
}

pub fn prepare(
    base: &Url,
    actor: &Actor,
    spec: &RequestSpec,
    root: &Path,
    execute: bool,
    allow_writes: bool,
) -> Result<PreparedRequest, String> {
    let method = match spec.method.as_str() {
        "GET" => Method::GET,
        "HEAD" => Method::HEAD,
        "POST" | "PUT" | "PATCH" | "DELETE" if allow_writes || !execute => {
            Method::from_bytes(spec.method.as_bytes()).map_err(|_| "method_unsupported")?
        }
        "POST" | "PUT" | "PATCH" | "DELETE" => {
            return Err("write_execution_requires_opt_in".into());
        }
        _ => return Err("method_unsupported".into()),
    };
    let url = target_url(base, &spec.path)?;
    if spec.body_env.is_some() && spec.body_file.is_some() {
        return Err("conflicting_body_bindings".into());
    }
    if (method == Method::GET || method == Method::HEAD)
        && (spec.body_env.is_some() || spec.body_file.is_some())
    {
        return Err("bodyless_method_has_body".into());
    }
    if let Some(status) = spec.expect_status
        && !(100..=599).contains(&status)
    {
        return Err("expected_status_invalid".into());
    }
    let response_schema = spec
        .response_schema
        .as_ref()
        .map(|schema| {
            crate::schema::compile(schema, schema).map_err(|_| "response_schema_unsupported")
        })
        .transpose()?;
    if method == Method::HEAD && response_schema.is_some() {
        return Err("head_response_schema_unsupported".into());
    }
    let mut headers = HeaderMap::new();
    if actor.headers_env.len() > MAX_HEADERS {
        return Err("too_many_actor_headers".into());
    }
    let mut header_bytes = 0;
    for (name, variable) in &actor.headers_env {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "header_name_invalid")?;
        if reserved_header(&name) {
            return Err("header_reserved".into());
        }
        validate_env_name(variable)?;
        let value = if execute {
            let secret = std::env::var(variable).map_err(|_| "header_environment_missing")?;
            secret_header(&secret)?
        } else {
            HeaderValue::from_static("preview")
        };
        insert_header(&mut headers, name, value, &mut header_bytes)?;
    }
    if let Some(content_type) = &spec.content_type {
        let value = HeaderValue::from_str(content_type).map_err(|_| "content_type_invalid")?;
        insert_header(&mut headers, CONTENT_TYPE, value, &mut header_bytes)?;
    }
    if let Some(variable) = &spec.idempotency_key_env {
        validate_env_name(variable)?;
        let value = if execute {
            let secret = std::env::var(variable).map_err(|_| "idempotency_environment_missing")?;
            secret_header(&secret)?
        } else {
            HeaderValue::from_static("preview")
        };
        insert_header(
            &mut headers,
            HeaderName::from_static("idempotency-key"),
            value,
            &mut header_bytes,
        )?;
    }
    if let Some(variable) = &spec.body_env {
        validate_env_name(variable)?;
    }
    if let Some(path) = &spec.body_file {
        validate_body_path(path)?;
    }
    let mut request = Request::new(method, url);
    *request.headers_mut() = headers;
    if execute {
        let body = if let Some(variable) = &spec.body_env {
            let body = std::env::var(variable).map_err(|_| "body_environment_missing")?;
            if body.len() > MAX_BODY_BYTES {
                return Err("body_exceeds_65536_byte_limit".into());
            }
            Some(body.into_bytes())
        } else if let Some(path) = &spec.body_file {
            Some(read_body(root, path)?)
        } else {
            None
        };
        if let Some(body) = body {
            *request.body_mut() = Some(body.into());
        }
    }
    Ok(PreparedRequest {
        request,
        expect_status: spec.expect_status,
        response_schema,
        executable: execute,
    })
}

fn reserved_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-authorization"
            | "proxy-connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "set-cookie"
    )
}

fn validate_env_name(variable: &str) -> Result<(), String> {
    let mut bytes = variable.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if variable.len() > 256
        || !valid_start
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("environment_binding_name_invalid".into());
    }
    Ok(())
}

fn secret_header(secret: &str) -> Result<HeaderValue, String> {
    if secret.is_empty() || secret.len() > MAX_HEADER_VALUE_BYTES {
        return Err("header_value_empty_or_exceeds_limit".into());
    }
    let mut value = HeaderValue::from_str(secret).map_err(|_| "header_value_invalid")?;
    value.set_sensitive(true);
    Ok(value)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: HeaderValue,
    total: &mut usize,
) -> Result<(), String> {
    if headers.contains_key(&name) {
        return Err("conflicting_request_headers".into());
    }
    *total += name.as_str().len() + value.as_bytes().len();
    if value.as_bytes().len() > MAX_HEADER_VALUE_BYTES
        || *total > MAX_HEADER_BYTES
        || headers.len() >= MAX_HEADERS
    {
        return Err("headers_exceed_limit".into());
    }
    headers.insert(name, value);
    Ok(())
}

fn target_url(base: &Url, path: &str) -> Result<Url, String> {
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("base_url_invalid".into());
    }
    validate_path(base.path(), false)?;
    validate_path(path, true)?;
    let prefix = base.path().trim_end_matches('/');
    let combined = format!("{prefix}{path}");
    // Parsing a complete URL preserves valid %-encoded path data. Setting path
    // segments would instead encode '%' again or permit URL-library dot removal.
    let origin = base.origin().ascii_serialization();
    let url = Url::parse(&format!("{origin}{combined}")).map_err(|_| "request_path_invalid")?;
    if url.origin() != base.origin()
        || url.path() != combined
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("request_path_invalid".into());
    }
    Ok(url)
}

fn validate_path(path: &str, request: bool) -> Result<(), String> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || !path.starts_with('/')
        || path.starts_with("//")
        || path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || path.contains(['\\', '?', '#'])
    {
        return Err("request_path_invalid".into());
    }
    for segment in path.split('/') {
        let bytes = segment.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let byte = if bytes[index] == b'%' {
                if index + 2 >= bytes.len() {
                    return Err("request_path_invalid".into());
                }
                let high = hex(bytes[index + 1]).ok_or("request_path_invalid")?;
                let low = hex(bytes[index + 2]).ok_or("request_path_invalid")?;
                index += 3;
                let byte = high * 16 + low;
                // Encoded separators/controls are rejected before a server can
                // reinterpret them as a new path or transport syntax.
                if matches!(byte, b'/' | b'\\') || byte.is_ascii_control() {
                    return Err("request_path_invalid".into());
                }
                byte
            } else {
                let byte = bytes[index];
                index += 1;
                byte
            };
            decoded.push(byte);
        }
        if matches!(decoded.as_slice(), b"." | b"..") {
            return Err("request_path_invalid".into());
        }
    }
    if request && path.contains("//") {
        return Err("request_path_invalid".into());
    }
    Ok(())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_body_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("body_file_path_invalid".into());
    }
    Ok(())
}

fn read_body(root: &Path, path: &str) -> Result<Vec<u8>, String> {
    let root = root.canonicalize().map_err(|_| "body_file_unavailable")?;
    let path = root
        .join(path)
        .canonicalize()
        .map_err(|_| "body_file_unavailable")?;
    if !path.starts_with(&root) {
        return Err("body_file_outside_workload_root".into());
    }
    let metadata = path.metadata().map_err(|_| "body_file_unavailable")?;
    if !metadata.is_file() {
        return Err("body_file_must_be_regular".into());
    }
    if metadata.len() > MAX_BODY_BYTES as u64 {
        return Err("body_exceeds_65536_byte_limit".into());
    }
    let mut body = Vec::new();
    File::open(path)
        .map_err(|_| "body_file_unavailable")?
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "body_file_unavailable")?;
    if body.len() > MAX_BODY_BYTES {
        return Err("body_exceeds_65536_byte_limit".into());
    }
    Ok(body)
}

pub async fn execute(client: &Client, prepared: &PreparedRequest) -> Outcome {
    if !prepared.executable {
        return failed(None, "preview_not_executable");
    }
    let Some(request) = prepared.request.try_clone() else {
        return failed(None, "request_not_replayable");
    };
    let method = request.method().clone();
    let mut response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return failed(None, "timeout"),
        Err(_) => return failed(None, "transport_error"),
    };
    let status = response.status().as_u16();
    if method != Method::HEAD
        && !matches!(status, 204 | 205 | 304)
        && response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return failed(Some(status), "response_exceeds_limit");
    }
    let mut body = Vec::new();
    let mut body_bytes = 0usize;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body_bytes.saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES => {
                body_bytes += chunk.len();
                if prepared.response_schema.is_some() {
                    body.extend_from_slice(&chunk);
                }
            }
            Ok(Some(_)) => return failed(Some(status), "response_exceeds_limit"),
            Ok(None) => break,
            Err(error) if error.is_timeout() => return failed(Some(status), "timeout"),
            Err(_) => return failed(Some(status), "response_body_error"),
        }
    }
    let accepted = prepared
        .expect_status
        .map_or((200..=299).contains(&status), |expected| status == expected);
    if !accepted {
        return rejected(status, "unexpected_status");
    }
    if method == Method::HEAD || matches!(status, 204 | 205 | 304) {
        if body_bytes != 0 {
            return rejected(status, "unexpected_body");
        }
        if prepared.response_schema.is_some() {
            return rejected(status, "response_schema_body_unavailable");
        }
    } else if let Some(validator) = &prepared.response_schema {
        let Ok(value) = crate::strict_json::parse_json_with_limit(&body, MAX_RESPONSE_BYTES) else {
            return rejected(status, "response_json_invalid");
        };
        if !validator.is_valid(&value) {
            return rejected(status, "response_schema_mismatch");
        }
    }
    Outcome {
        passed: true,
        status: Some(status),
        reason: "passed",
        response_complete: true,
    }
}

fn failed(status: Option<u16>, reason: &'static str) -> Outcome {
    Outcome {
        passed: false,
        status,
        reason,
        response_complete: false,
    }
}

fn rejected(status: u16, reason: &'static str) -> Outcome {
    Outcome {
        response_complete: true,
        ..failed(Some(status), reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread,
        time::Instant,
    };

    fn spec(method: &str, path: &str) -> RequestSpec {
        RequestSpec {
            method: method.into(),
            path: path.into(),
            body_env: None,
            body_file: None,
            content_type: None,
            idempotency_key_env: None,
            expect_status: None,
            response_schema: None,
        }
    }

    fn actor() -> Actor {
        Actor {
            headers_env: BTreeMap::new(),
        }
    }

    fn base() -> Url {
        Url::parse("http://127.0.0.1:1/api/v1").unwrap()
    }

    fn preview(spec: &RequestSpec) -> Result<PreparedRequest, String> {
        prepare(&base(), &actor(), spec, Path::new("/missing"), false, false)
    }

    #[test]
    fn prepared_requests_can_be_shared_without_mutable_actor_state() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<PreparedRequest>();
    }

    #[test]
    fn paths_keep_base_prefix_and_encoded_data() {
        for (path, expected) in [
            ("/widgets", "/api/v1/widgets"),
            (
                "/widgets/red%3Bblue%2Cgreen",
                "/api/v1/widgets/red%3Bblue%2Cgreen",
            ),
            ("/widgets/50%25", "/api/v1/widgets/50%25"),
        ] {
            let plan = preview(&spec("GET", path)).unwrap();
            assert_eq!(plan.request.url().path(), expected);
        }
        let prefixed = Url::parse("http://127.0.0.1:1/api/v1/").unwrap();
        assert_eq!(
            target_url(&prefixed, "/widgets").unwrap().path(),
            "/api/v1/widgets"
        );
    }

    #[test]
    fn path_escape_and_ambiguous_percent_encodings_are_rejected() {
        for path in [
            "https://other.invalid/",
            "//other.invalid/",
            "relative",
            "/../escape",
            "/a/./b",
            "/%2e%2e/escape",
            "/a/.%2E/b",
            "/a/%2f/b",
            "/a/%5C/b",
            "/a\\b",
            "/a?secret=value",
            "/a#fragment",
            "/a/%00",
            "/a/%0a",
            "/a/%",
            "/a/%xy",
            "/a//b",
            "/a b",
        ] {
            assert!(preview(&spec("GET", path)).is_err(), "accepted {path}");
        }
        for url in [
            "ftp://example.invalid/",
            "http://user:pass@example.invalid/",
            "http://example.invalid/?token=x",
            "http://example.invalid/#x",
        ] {
            assert!(
                prepare(
                    &Url::parse(url).unwrap(),
                    &actor(),
                    &spec("GET", "/"),
                    Path::new("."),
                    false,
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn preview_does_not_read_credentials_or_body_files() {
        let actor = Actor {
            headers_env: BTreeMap::from([(
                "Cookie".into(),
                "RUSTA_NONEXISTENT_SECRET_SENTINEL".into(),
            )]),
        };
        let mut request = spec("POST", "/items");
        request.body_file = Some("not-created/body.bin".into());
        request.idempotency_key_env = Some("RUSTA_NONEXISTENT_KEY_SENTINEL".into());
        let plan = prepare(
            &base(),
            &actor,
            &request,
            Path::new("/missing"),
            false,
            true,
        )
        .unwrap();
        assert!(!plan.executable);
        assert!(plan.request.body().is_none());
        assert_eq!(plan.request.headers()["cookie"], "preview");
        request.body_file = None;
        request.body_env = Some("RUSTA_NONEXISTENT_BODY_SENTINEL".into());
        assert!(
            prepare(
                &base(),
                &actor,
                &request,
                Path::new("/missing"),
                false,
                true
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn preview_plan_cannot_accidentally_execute() {
        let plan = preview(&spec("GET", "/items")).unwrap();
        let outcome = execute(&client(100).unwrap(), &plan).await;
        assert!(!outcome.passed);
        assert_eq!(outcome.status, None);
        assert_eq!(outcome.reason, "preview_not_executable");
        assert!(!outcome.response_complete);
    }

    #[test]
    fn writes_and_body_bindings_require_explicit_valid_configuration() {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let request = spec(method, "/items");
            assert!(preview(&request).is_ok());
            assert_eq!(
                prepare(&base(), &actor(), &request, Path::new("."), true, false)
                    .err()
                    .unwrap(),
                "write_execution_requires_opt_in"
            );
            assert!(prepare(&base(), &actor(), &request, Path::new("."), false, true).is_ok());
        }
        for method in ["CONNECT", "OPTIONS", "TRACE", "get"] {
            assert_eq!(
                preview(&spec(method, "/items")).err().unwrap(),
                "method_unsupported"
            );
        }
        let mut request = spec("GET", "/items");
        request.body_env = Some("BODY".into());
        assert_eq!(preview(&request).err().unwrap(), "bodyless_method_has_body");
        request.method = "POST".into();
        request.body_file = Some("body.bin".into());
        assert_eq!(
            prepare(&base(), &actor(), &request, Path::new("."), false, true)
                .err()
                .unwrap(),
            "conflicting_body_bindings"
        );
    }

    #[test]
    fn headers_reject_case_insensitive_collisions_and_transport_overrides() {
        for header in [
            "Host",
            "Content-Length",
            "Transfer-Encoding",
            "Connection",
            "Proxy-Authorization",
            "TE",
            "Upgrade",
            "Set-Cookie",
        ] {
            let actor = Actor {
                headers_env: BTreeMap::from([(header.into(), "TOKEN".into())]),
            };
            assert_eq!(
                prepare(
                    &base(),
                    &actor,
                    &spec("GET", "/"),
                    Path::new("."),
                    false,
                    false
                )
                .err()
                .unwrap(),
                "header_reserved"
            );
        }
        let actor = Actor {
            headers_env: BTreeMap::from([
                ("Cookie".into(), "ONE".into()),
                ("cookie".into(), "TWO".into()),
            ]),
        };
        assert_eq!(
            prepare(
                &base(),
                &actor,
                &spec("GET", "/"),
                Path::new("."),
                false,
                false
            )
            .err()
            .unwrap(),
            "conflicting_request_headers"
        );
        let actor = Actor {
            headers_env: BTreeMap::from([("Content-Type".into(), "TYPE".into())]),
        };
        let mut request = spec("GET", "/");
        request.content_type = Some("application/json".into());
        assert_eq!(
            prepare(&base(), &actor, &request, Path::new("."), false, false)
                .err()
                .unwrap(),
            "conflicting_request_headers"
        );
        let actor = Actor {
            headers_env: BTreeMap::from([("Idempotency-Key".into(), "ONE".into())]),
        };
        request.content_type = None;
        request.idempotency_key_env = Some("TWO".into());
        assert_eq!(
            prepare(&base(), &actor, &request, Path::new("."), false, false)
                .err()
                .unwrap(),
            "conflicting_request_headers"
        );
    }

    #[test]
    fn invalid_headers_and_bindings_return_only_generic_errors() {
        let sentinel = "SECRET_SENTINEL";
        let error = secret_header(&format!("{sentinel}\r\nX-Injected: yes"))
            .err()
            .unwrap();
        assert_eq!(error, "header_value_invalid");
        assert!(!error.contains(sentinel));
        assert!(secret_header("").is_err());
        assert!(secret_header(&"x".repeat(MAX_HEADER_VALUE_BYTES + 1)).is_err());
        let actor = Actor {
            headers_env: BTreeMap::from([(
                "Cookie".into(),
                "RUSTA_NONEXISTENT_SECRET_SENTINEL".into(),
            )]),
        };
        let error = prepare(
            &base(),
            &actor,
            &spec("GET", "/"),
            Path::new("."),
            true,
            false,
        )
        .err()
        .unwrap();
        assert_eq!(error, "header_environment_missing");
        assert!(!error.contains(sentinel));
        let actor = Actor {
            headers_env: BTreeMap::from([("X-Bad\nName".into(), "TOKEN".into())]),
        };
        assert!(
            prepare(
                &base(),
                &actor,
                &spec("GET", "/"),
                Path::new("."),
                false,
                false
            )
            .is_err()
        );
        for variable in ["", "9TOKEN", "TOKEN=value", "TOKEN\n"] {
            assert!(validate_env_name(variable).is_err());
        }
    }

    #[test]
    fn header_and_body_limits_are_bounded() {
        let actor = Actor {
            headers_env: (0..=MAX_HEADERS)
                .map(|index| (format!("X-{index}"), "TOKEN".into()))
                .collect(),
        };
        assert_eq!(
            prepare(
                &base(),
                &actor,
                &spec("GET", "/"),
                Path::new("."),
                false,
                false
            )
            .err()
            .unwrap(),
            "too_many_actor_headers"
        );
        let mut headers = HeaderMap::new();
        let mut total = 0;
        for index in 0..8 {
            let result = insert_header(
                &mut headers,
                HeaderName::from_bytes(format!("x-{index}").as_bytes()).unwrap(),
                HeaderValue::from_bytes(&vec![b'x'; MAX_HEADER_VALUE_BYTES]).unwrap(),
                &mut total,
            );
            assert_eq!(result.is_ok(), index < 7);
        }
        let directory = TempDir::new();
        std::fs::write(directory.0.join("large.bin"), vec![0; MAX_BODY_BYTES + 1]).unwrap();
        assert_eq!(
            read_body(&directory.0, "large.bin").err().unwrap(),
            "body_exceeds_65536_byte_limit"
        );
        for path in ["", "/absolute", "../outside", "a/../outside", "./body.bin"] {
            assert!(validate_body_path(path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn body_file_symlinks_cannot_leave_the_workload_directory() {
        let directory = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.0.join("outside.bin"), b"synthetic outside data").unwrap();
        std::os::unix::fs::symlink(outside.0.join("outside.bin"), directory.0.join("body.bin"))
            .unwrap();
        assert_eq!(
            read_body(&directory.0, "body.bin").err().unwrap(),
            "body_file_outside_workload_root"
        );
    }

    #[test]
    fn response_schema_validation_uses_only_local_supported_schema() {
        let mut request = spec("GET", "/");
        request.response_schema = Some(
            serde_json::json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}),
        );
        assert!(preview(&request).is_ok());
        request.response_schema =
            Some(serde_json::json!({"$ref":"https://example.invalid/schema"}));
        assert_eq!(
            preview(&request).err().unwrap(),
            "response_schema_unsupported"
        );
        request.response_schema =
            Some(serde_json::json!({"$defs":{"flag":{"type":"boolean"}},"$ref":"#/$defs/flag"}));
        assert!(preview(&request).is_ok());
        request.method = "HEAD".into();
        assert_eq!(
            preview(&request).err().unwrap(),
            "head_response_schema_unsupported"
        );
    }

    #[derive(Clone)]
    struct Response {
        status: u16,
        body: Vec<u8>,
        delay_ms: u64,
        headers: &'static str,
        chunked: bool,
    }

    #[derive(Debug)]
    struct Captured {
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    struct Server {
        url: Url,
        captured: Arc<Mutex<Vec<Captured>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(response: Response) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url =
                Url::parse(&format!("http://{}/prefix", listener.local_addr().unwrap())).unwrap();
            let captured = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&captured);
            let stop = Arc::new(AtomicBool::new(false));
            let should_stop = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                let started = Instant::now();
                while !should_stop.load(Ordering::Relaxed)
                    && started.elapsed() < Duration::from_secs(10)
                {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            handle(stream, &response, &shared);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("loopback fixture failed: {error}"),
                    }
                }
            });
            Self {
                url,
                captured,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let joined = self.worker.take().unwrap().join();
            if !thread::panicking() {
                joined.unwrap();
            }
        }
    }

    fn handle(mut stream: TcpStream, response: &Response, captured: &Mutex<Vec<Captured>>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let method = line.split_whitespace().next().unwrap().to_string();
        let path = line.split_whitespace().nth(1).unwrap().to_string();
        let mut headers = BTreeMap::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
        }
        let length = headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        captured.lock().unwrap().push(Captured {
            path,
            headers,
            body,
        });
        thread::sleep(Duration::from_millis(response.delay_ms));
        let framing = if response.chunked {
            "Transfer-Encoding: chunked".to_string()
        } else {
            format!("Content-Length: {}", response.body.len())
        };
        let head = format!(
            "HTTP/1.1 {} Fixture\r\n{framing}\r\nConnection: close\r\n{}\r\n",
            response.status, response.headers
        );
        let _ = stream.write_all(head.as_bytes());
        if method == "HEAD" {
            return;
        }
        if response.chunked {
            for chunk in response.body.chunks(4096) {
                if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                    || stream.write_all(chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        } else {
            let _ = stream.write_all(&response.body);
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusta-workload-http-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn response(status: u16, body: &[u8]) -> Response {
        Response {
            status,
            body: body.into(),
            delay_ms: 0,
            headers: "",
            chunked: false,
        }
    }

    #[tokio::test]
    async fn explicit_denial_is_a_pass_but_is_not_the_default() {
        let server = Server::start(response(403, b"denied"));
        let mut request = spec("GET", "/denied");
        request.expect_status = Some(403);
        let plan = prepare(&server.url, &actor(), &request, Path::new("."), true, false).unwrap();
        let http = client(500).unwrap();
        let outcome = execute(&http, &plan).await;
        assert!(outcome.passed);
        assert_eq!(outcome.status, Some(403));
        assert!(outcome.response_complete);
        request.expect_status = None;
        let plan = prepare(&server.url, &actor(), &request, Path::new("."), true, false).unwrap();
        assert_eq!(execute(&http, &plan).await.reason, "unexpected_status");
        assert_eq!(server.captured.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_status_and_redirect_are_never_retried_or_followed() {
        for status in [302, 503] {
            let mut response = response(status, b"private response sentinel");
            response.headers = "Location: http://127.0.0.1:1/private-token\r\n";
            let server = Server::start(response);
            let plan = prepare(
                &server.url,
                &actor(),
                &spec("GET", "/items"),
                Path::new("."),
                true,
                false,
            )
            .unwrap();
            let outcome = execute(&client(500).unwrap(), &plan).await;
            assert_eq!(outcome.reason, "unexpected_status");
            assert_eq!(outcome.status, Some(status));
            assert!(outcome.response_complete);
            assert_eq!(server.captured.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn schemas_and_response_bounds_return_safe_outcomes() {
        for (body, reason) in [
            (b"{\"ok\":true}".to_vec(), "passed"),
            (
                b"{\"ok\":\"SECRET_SENTINEL\"}".to_vec(),
                "response_schema_mismatch",
            ),
            (b"SECRET_SENTINEL".to_vec(), "response_json_invalid"),
            (vec![b'x'; MAX_RESPONSE_BYTES + 1], "response_exceeds_limit"),
        ] {
            let server = Server::start(response(200, &body));
            let mut request = spec("GET", "/items");
            request.response_schema = Some(
                serde_json::json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}),
            );
            let plan =
                prepare(&server.url, &actor(), &request, Path::new("."), true, false).unwrap();
            let outcome = execute(&client(500).unwrap(), &plan).await;
            assert_eq!(outcome.reason, reason);
            assert!(!outcome.reason.contains("SECRET_SENTINEL"));
            assert_eq!(
                outcome.response_complete,
                reason != "response_exceeds_limit"
            );
        }
    }

    #[tokio::test]
    async fn chunked_response_limit_is_enforced_without_a_length_header() {
        let mut fixture = response(200, &vec![b'x'; MAX_RESPONSE_BYTES + 1]);
        fixture.chunked = true;
        let server = Server::start(fixture);
        let plan = prepare(
            &server.url,
            &actor(),
            &spec("GET", "/items"),
            Path::new("."),
            true,
            false,
        )
        .unwrap();
        let outcome = execute(&client(1000).unwrap(), &plan).await;
        assert_eq!(outcome.reason, "response_exceeds_limit");
        assert!(!outcome.response_complete);
        assert_eq!(server.captured.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn body_timeout_is_counted_once_and_head_needs_no_json() {
        let mut delayed = response(200, b"ok");
        delayed.delay_ms = 200;
        let server = Server::start(delayed);
        let plan = prepare(
            &server.url,
            &actor(),
            &spec("GET", "/items"),
            Path::new("."),
            true,
            false,
        )
        .unwrap();
        assert_eq!(execute(&client(50).unwrap(), &plan).await.reason, "timeout");
        assert_eq!(server.captured.lock().unwrap().len(), 1);
        // HEAD Content-Length describes a representation, not a response body.
        let server = Server::start(response(200, &vec![b'x'; MAX_RESPONSE_BYTES + 1]));
        let plan = prepare(
            &server.url,
            &actor(),
            &spec("HEAD", "/items"),
            Path::new("."),
            true,
            false,
        )
        .unwrap();
        assert!(execute(&client(500).unwrap(), &plan).await.passed);
    }

    // Pass environment values to a child process rather than mutating the
    // multithreaded Rust test process's environment (unsafe in edition 2024).
    #[test]
    fn actor_isolation_and_exact_replay_with_runtime_bindings() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workload_http::tests::runtime_binding_child",
                "--nocapture",
            ])
            .env("RUSTA_HTTP_TEST_CHILD", "1")
            .env("RUSTA_HTTP_COOKIE_A", "session=synthetic-actor-a")
            .env("RUSTA_HTTP_COOKIE_B", "session=synthetic-actor-b")
            .env("RUSTA_HTTP_KEY", "synthetic-logical-write-1")
            .env("RUSTA_HTTP_BODY", "synthetic body bytes")
            .env("RUSTA_HTTP_LARGE_BODY", "x".repeat(MAX_BODY_BYTES + 1))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn runtime_binding_child() {
        if std::env::var_os("RUSTA_HTTP_TEST_CHILD").is_none() {
            return;
        }
        let mut fixture = response(200, b"ok");
        fixture.headers = "Set-Cookie: session=synthetic-server-update\r\n";
        let server = Server::start(fixture);
        let directory = TempDir::new();
        let bytes = b"immutable\0body\xffbytes";
        std::fs::write(directory.0.join("body.bin"), bytes).unwrap();
        let mut request = spec("POST", "/write");
        request.body_file = Some("body.bin".into());
        request.content_type = Some("application/octet-stream".into());
        request.idempotency_key_env = Some("RUSTA_HTTP_KEY".into());
        let actor_a = Actor {
            headers_env: BTreeMap::from([("Cookie".into(), "RUSTA_HTTP_COOKIE_A".into())]),
        };
        let actor_b = Actor {
            headers_env: BTreeMap::from([("Cookie".into(), "RUSTA_HTTP_COOKIE_B".into())]),
        };
        let plan_a = prepare(&server.url, &actor_a, &request, &directory.0, true, true).unwrap();
        let plan_b = prepare(&server.url, &actor_b, &request, &directory.0, true, true).unwrap();
        std::fs::write(directory.0.join("body.bin"), b"modified after preflight").unwrap();
        let http = client(500).unwrap();
        assert!(execute(&http, &plan_a).await.passed);
        assert!(execute(&http, &plan_b).await.passed);
        assert!(execute(&http, &plan_a).await.passed);
        {
            let captured = server.captured.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert_eq!(captured[0].headers["cookie"], "session=synthetic-actor-a");
            assert_eq!(captured[1].headers["cookie"], "session=synthetic-actor-b");
            assert_eq!(captured[2].headers["cookie"], "session=synthetic-actor-a");
            for captured in captured.iter() {
                assert_eq!(captured.path, "/prefix/write");
                assert_eq!(captured.body, bytes);
                assert_eq!(
                    captured.headers["idempotency-key"],
                    "synthetic-logical-write-1"
                );
            }
        }
        request.body_file = None;
        request.body_env = Some("RUSTA_HTTP_BODY".into());
        let plan = prepare(&server.url, &actor_a, &request, &directory.0, true, true).unwrap();
        assert!(execute(&http, &plan).await.passed);
        assert_eq!(
            server.captured.lock().unwrap()[3].body,
            b"synthetic body bytes"
        );
        request.body_env = Some("RUSTA_HTTP_LARGE_BODY".into());
        assert_eq!(
            prepare(&server.url, &actor_a, &request, &directory.0, true, true)
                .err()
                .unwrap(),
            "body_exceeds_65536_byte_limit"
        );
    }
}
