use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const ROUTES: [&str; 4] = [
    "/sleep/tokio",
    "/sleep/tokio-spawn",
    "/sleep/thread",
    "/sleep/park",
];

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-pitest-contract-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct Request {
    path: String,
    body: Value,
}

struct Response {
    status: u16,
    body: Vec<u8>,
    retry_after: Vec<&'static str>,
    declared_length: Option<usize>,
}

impl Response {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(&body).unwrap(),
            retry_after: if matches!(status, 429 | 503) {
                vec!["1"]
            } else {
                vec![]
            },
            declared_length: None,
        }
    }

    fn write(self, stream: &mut TcpStream) {
        write!(
            stream,
            "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.declared_length.unwrap_or(self.body.len())
        )
        .unwrap();
        for value in self.retry_after {
            write!(stream, "Retry-After: {value}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(&self.body).unwrap();
    }
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Vec<Request>>,
}

impl Server {
    fn start(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("test server accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let request = Request { path, body };
                handler(&request).write(&mut stream);
                requests.push(request);
            }
            requests
        });
        Self { url, stop, worker }
    }

    fn finish(self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.join().unwrap()
    }
}

fn run(directory: &TempDir, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .current_dir(&directory.0)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "CLI did not finish: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn profile(server: &Server, extra: &[&str], validate: bool) -> (Output, Value) {
    profile_with_requests(server, extra, validate, 1)
}

fn profile_with_requests(
    server: &Server,
    extra: &[&str],
    validate: bool,
    requests: u64,
) -> (Output, Value) {
    let directory = TempDir::new();
    let request_count = requests.to_string();
    let mut args = vec![
        "profile",
        "sleepapi",
        "--base-url",
        &server.url,
        "--requests",
        &request_count,
        "--concurrency",
        "1",
        "--output",
        "result.json",
    ];
    if validate {
        args.push("--validate-contract");
    }
    args.extend_from_slice(extra);
    let output = run(&directory, &args);
    let path = directory.0.join("result.json");
    let document = serde_json::from_slice(
        &fs::read(&path).unwrap_or_else(|error| panic!("missing report: {error}; {output:?}")),
    )
    .unwrap();
    (output, document)
}

// Expected fixture values come from SleepAPI's response and ErrorDetails
// contract, independently of the benchmark runner's validation implementation.
fn response_body(route: &str, status: u16, delay: u32, scenario: Option<&str>) -> Value {
    let (method, blocks_thread) = match route {
        "/sleep/tokio" => ("tokio_sleep", false),
        "/sleep/tokio-spawn" => ("tokio_spawn", false),
        "/sleep/thread" => ("thread_sleep", true),
        "/sleep/park" => ("thread_park", true),
        _ => panic!("unexpected route {route}"),
    };
    let mut body = json!({
        "operation": "sleep", "simulated": true, "method": method,
        "blocks_thread": blocks_thread, "requested_duration_ms": delay,
        "elapsed_ms": delay, "status_code": status,
    });
    if let Some(scenario) = scenario {
        body["scenario"] = json!(scenario);
    }
    if status >= 400 {
        let code = match status {
            400 => "invalid_request",
            401 => "unauthorized",
            403 => "forbidden",
            404 => "not_found",
            405 => "method_not_allowed",
            408 => "request_timeout",
            409 => "conflict",
            413 => "payload_too_large",
            415 => "unsupported_media_type",
            422 => "validation_failed",
            429 => "rate_limited",
            500 => "internal_error",
            502 => "bad_gateway",
            503 => "service_unavailable",
            504 => "upstream_timeout",
            _ if (400..500).contains(&status) => "client_error",
            _ => "server_error",
        };
        body["error"] = json!({
            "code": code, "message": "Simulated provider response.",
            "retryable": matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
        });
    }
    body
}

fn assert_contract(run: &Value, checked: u64, passed: u64, failed: u64) {
    let contract = &run["report"]["contract_validation"];
    assert_eq!(contract["scope"], "all_completed_responses");
    assert_eq!(contract["body_limit_bytes"], 65_536);
    assert_eq!(contract["checked"], checked);
    assert_eq!(contract["passed"], passed);
    assert_eq!(contract["failed"], failed);
    assert_eq!(checked, passed + failed);
    if failed == 0 {
        assert_eq!(contract["failures"], json!({}));
    }
}

fn assert_passing_profile(output: &Output, document: &Value) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(document["passed"], true);
    let runs = document["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 4);
    for (run, route) in runs.iter().zip(ROUTES) {
        assert_eq!(run["route"], route);
        assert_eq!(run["passed"], true);
        assert_eq!(run["report"]["metrics"]["succeeded"], 1);
        assert_contract(run, 1, 1, 0);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout
            .lines()
            .filter(|line| *line == "Contract checks: 1")
            .count(),
        4
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| *line == "Contract failures: 0")
            .count(),
        4
    );
}

#[test]
fn all_routes_validate_default_and_named_scenario_responses() {
    for (scenario, delay, status) in [
        (None, 400, 200),
        (Some("fast_success"), 50, 200),
        (Some("slow_provider"), 2000, 200),
        (Some("delayed_failure"), 800, 503),
        (Some("client_timeout"), 3000, 200),
        (Some("rate_limited"), 100, 429),
        (Some("invalid_request"), 50, 400),
        (Some("upstream_timeout"), 2000, 504),
    ] {
        let server = Server::start(move |request| {
            Response::json(
                status,
                response_body(&request.path, status, delay, scenario),
            )
        });
        let extra = scenario.map_or(vec![], |name| vec!["--scenario", name]);
        let (output, document) = profile(&server, &extra, true);
        let requests = server.finish();
        assert_passing_profile(&output, &document);
        assert_eq!(requests.len(), 4);
        for request in requests {
            assert_eq!(
                request.body,
                match scenario {
                    Some(name) => json!({"scenario": name}),
                    None => json!({"duration_ms": 400, "status_code": 200}),
                }
            );
        }
    }
}

#[test]
fn error_contract_uses_the_sleepapi_status_mapping_and_retryability() {
    for status in [
        400, 401, 403, 404, 405, 408, 409, 413, 415, 422, 429, 499, 500, 501, 502, 503, 504, 599,
    ] {
        let server = Server::start(move |request| {
            Response::json(status, response_body(&request.path, status, 0, None))
        });
        let (output, document) = profile(
            &server,
            &["--status-code", &status.to_string(), "--delay-ms", "0"],
            true,
        );
        assert_eq!(server.finish().len(), 4);
        assert_passing_profile(&output, &document);
    }
}

#[test]
fn contract_mismatches_fail_without_changing_http_success_counts() {
    for (mutation, reason, status, scenario) in [
        ("operation", "operation", 200, None),
        ("missing_operation", "operation", 200, None),
        ("method", "method", 200, None),
        ("missing_method", "method", 200, None),
        ("blocks_thread", "blocks_thread", 200, None),
        ("duration", "requested_duration_ms", 200, None),
        ("duration_type", "requested_duration_ms", 200, None),
        ("status_code", "status_code", 200, None),
        ("simulated", "simulated", 200, None),
        ("missing_simulated", "simulated", 200, None),
        ("unexpected_scenario", "scenario", 200, None),
        ("missing_scenario", "scenario", 200, Some("fast_success")),
        ("wrong_scenario", "scenario", 200, Some("fast_success")),
        ("success_error", "error", 200, None),
        ("missing_error", "error", 503, None),
        ("error_code", "error.code", 503, None),
        ("retryable", "error.retryable", 503, None),
        ("missing_retryable", "error.retryable", 503, None),
        ("missing_retry_after", "retry_after", 503, None),
        ("wrong_retry_after", "retry_after", 429, None),
        ("duplicate_retry_after", "retry_after", 429, None),
        ("unexpected_retry_after", "retry_after", 200, None),
        ("invalid_json", "invalid_json", 200, None),
        ("scalar_json", "invalid_json", 200, None),
    ] {
        // One case mixes a failed response with two valid responses on
        // the same route, proving every response contributes to the totals.
        let requests = if mutation == "operation" { 3 } else { 1 };
        let first_route_requests = AtomicU64::new(0);
        let server = Server::start(move |request| {
            let mut body = response_body(&request.path, status, 0, scenario);
            let mut response = Response::json(status, body.clone());
            if request.path != ROUTES[0]
                || first_route_requests.fetch_add(1, Ordering::Relaxed) != 0
            {
                return response;
            }
            match mutation {
                "operation" => body["operation"] = json!("email"),
                "missing_operation" => {
                    body.as_object_mut().unwrap().remove("operation");
                }
                "method" => body["method"] = json!("thread_sleep"),
                "missing_method" => {
                    body.as_object_mut().unwrap().remove("method");
                }
                "blocks_thread" => body["blocks_thread"] = json!(true),
                "duration" => body["requested_duration_ms"] = json!(1),
                "duration_type" => body["requested_duration_ms"] = json!("0"),
                "status_code" => body["status_code"] = json!(201),
                "simulated" => body["simulated"] = json!(false),
                "missing_simulated" => {
                    body.as_object_mut().unwrap().remove("simulated");
                }
                "unexpected_scenario" | "wrong_scenario" => {
                    body["scenario"] = json!("slow_provider")
                }
                "missing_scenario" => {
                    body.as_object_mut().unwrap().remove("scenario");
                }
                "success_error" => {
                    body["error"] = json!({"code": "internal_error", "retryable": true})
                }
                "missing_error" => {
                    body.as_object_mut().unwrap().remove("error");
                }
                "error_code" => body["error"]["code"] = json!("rate_limited"),
                "retryable" => body["error"]["retryable"] = json!(false),
                "missing_retryable" => {
                    body["error"].as_object_mut().unwrap().remove("retryable");
                }
                "missing_retry_after" => response.retry_after.clear(),
                "wrong_retry_after" => response.retry_after = vec!["2"],
                "duplicate_retry_after" => response.retry_after = vec!["1", "1"],
                "unexpected_retry_after" => response.retry_after = vec!["1"],
                "invalid_json" | "scalar_json" => (),
                _ => unreachable!(),
            }
            response.body = match mutation {
                "invalid_json" => b"{invalid".to_vec(),
                "scalar_json" => b"true".to_vec(),
                _ => serde_json::to_vec(&body).unwrap(),
            };
            response
        });
        let status_arg = status.to_string();
        let mut extra = vec!["--delay-ms", "0"];
        if let Some(scenario) = scenario {
            extra.extend(["--scenario", scenario]);
        } else {
            extra.extend(["--status-code", &status_arg]);
        }
        let (output, document) = profile_with_requests(&server, &extra, true, requests);
        assert_eq!(server.finish().len() as u64, 4 * requests, "{mutation}");
        assert_eq!(output.status.code(), Some(1), "{mutation}: {output:?}");
        assert_eq!(document["passed"], false, "{mutation}");
        let runs = document["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 4);
        for (index, run) in runs.iter().enumerate() {
            assert_eq!(
                run["report"]["metrics"]["succeeded"], requests,
                "{mutation}"
            );
            assert_eq!(run["report"]["metrics"]["unexpected_status"], 0);
            assert_eq!(run["passed"], index != 0);
            let failures = u64::from(index == 0);
            assert_contract(run, requests, requests - failures, failures);
        }
        assert_eq!(
            runs[0]["report"]["contract_validation"]["failures"][reason], 1,
            "{mutation}: {}",
            runs[0]
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Contract failure reasons:"));
    }
}

#[test]
fn capacity_rejection_cannot_pass_as_a_rate_limited_simulation() {
    let server = Server::start(|request| {
        let body = if request.path == ROUTES[0] {
            json!({
                "simulated": false, "status_code": 429,
                "error": {"code": "capacity_exceeded", "message": "Busy", "retryable": true},
            })
        } else {
            response_body(&request.path, 429, 100, Some("rate_limited"))
        };
        Response::json(429, body)
    });
    let (output, document) = profile(&server, &["--scenario", "rate_limited"], true);
    assert_eq!(server.finish().len(), 4);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let first = &document["runs"][0];
    assert_eq!(first["report"]["metrics"]["succeeded"], 1);
    assert_contract(first, 1, 0, 1);
    assert_eq!(
        first["report"]["contract_validation"]["failures"],
        json!({"capacity_exceeded": 1})
    );
    for run in document["runs"].as_array().unwrap().iter().skip(1) {
        assert_contract(run, 1, 1, 0);
    }
}

#[test]
fn contract_body_limit_is_inclusive_and_oversized_bodies_are_still_completed() {
    for length in [65_536, 65_537] {
        let server = Server::start(move |request| {
            let mut body = response_body(&request.path, 200, 0, None);
            if request.path == ROUTES[0] {
                body["padding"] = json!("");
                let base_length = serde_json::to_vec(&body).unwrap().len();
                body["padding"] = json!("x".repeat(length - base_length));
            }
            let response = Response::json(200, body);
            if request.path == ROUTES[0] {
                assert_eq!(response.body.len(), length);
            }
            response
        });
        let (output, document) = profile(&server, &["--delay-ms", "0"], true);
        assert_eq!(server.finish().len(), 4);
        let oversized = length > 65_536;
        assert_eq!(
            output.status.code(),
            Some(i32::from(oversized)),
            "{output:?}"
        );
        assert_eq!(document["passed"], !oversized);
        let first = &document["runs"][0];
        assert_eq!(first["report"]["metrics"]["complete_responses"], 1);
        assert_eq!(
            first["report"]["metrics"]["status_counts"],
            json!({"200": 1})
        );
        assert_contract(first, 1, u64::from(!oversized), u64::from(oversized));
        if oversized {
            assert_eq!(
                first["report"]["contract_validation"]["failures"],
                json!({"body_too_large": 1})
            );
        }
    }
}

#[test]
fn contract_expectations_are_independent_of_http_status_overrides() {
    for (configured, actual, expected) in [(503, 503, 200), (200, 201, 201)] {
        let server = Server::start(move |request| {
            Response::json(actual, response_body(&request.path, actual, 0, None))
        });
        let (output, document) = profile(
            &server,
            &[
                "--delay-ms",
                "0",
                "--status-code",
                &configured.to_string(),
                "--expect-status",
                &expected.to_string(),
            ],
            true,
        );
        assert_eq!(server.finish().len(), 4);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(document["passed"], false);
        for run in document["runs"].as_array().unwrap() {
            let http_passed = actual == expected;
            let contract_passed = actual == configured;
            assert_eq!(
                run["report"]["metrics"]["succeeded"],
                u64::from(http_passed)
            );
            assert_eq!(
                run["report"]["metrics"]["unexpected_status"],
                u64::from(!http_passed)
            );
            assert_contract(
                run,
                1,
                u64::from(contract_passed),
                u64::from(!contract_passed),
            );
        }
    }
}

#[test]
fn incomplete_responses_are_not_checked_and_disabled_validation_keeps_status_only_behavior() {
    let server = Server::start(|request| {
        let mut response = Response::json(200, response_body(&request.path, 200, 0, None));
        if request.path == ROUTES[0] {
            response.body = b"{".to_vec();
            response.declared_length = Some(100);
        }
        response
    });
    let (output, document) = profile(&server, &["--delay-ms", "0"], true);
    assert_eq!(server.finish().len(), 4);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let first = &document["runs"][0];
    assert_eq!(first["report"]["metrics"]["transport_errors"], 1);
    assert_contract(first, 0, 0, 0);

    let server = Server::start(|_| Response::json(200, json!({})));
    let (output, document) = profile(&server, &["--delay-ms", "0"], false);
    assert_eq!(server.finish().len(), 4);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(document["passed"], true);
    for run in document["runs"].as_array().unwrap() {
        assert!(run["report"].get("contract_validation").is_none());
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Contract checks:"));
}

#[test]
fn unsupported_simulation_statuses_fail_before_traffic_and_supported_neighbors_parse() {
    let directory = TempDir::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for status in [204, 205, 206, 226, 304, 407, 416, 426] {
        let output = run(
            &directory,
            &[
                "profile",
                "sleepapi",
                "--base-url",
                &url,
                "--status-code",
                &status.to_string(),
                "--validate-contract",
            ],
        );
        assert_eq!(output.status.code(), Some(2), "status {status}: {output:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for status in [
        200, 203, 207, 225, 227, 303, 305, 406, 408, 415, 417, 425, 427, 599,
    ] {
        let output = run(
            &directory,
            &[
                "profile",
                "sleepapi",
                "--base-url",
                &url,
                "--status-code",
                &status.to_string(),
                "--validate-contract",
                "--dry-run",
            ],
        );
        assert_eq!(output.status.code(), Some(0), "status {status}: {output:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
