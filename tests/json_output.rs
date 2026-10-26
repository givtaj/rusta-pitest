use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-pitest-json-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn report(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("result.json")).unwrap()).unwrap()
    }

    fn files(&self) -> Vec<String> {
        let mut names = fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Vec<Request>>,
}

impl Server {
    fn start(handler: impl Fn(&Request, &mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requests = Vec::new();
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
                let mut start = line.split_whitespace();
                let method = start.next().unwrap().to_owned();
                let path = start.next().unwrap().to_owned();
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
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = Request { method, path, body };
                handler(&request, &mut stream);
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

fn respond(stream: &mut TcpStream, status: u16) {
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
    )
    .unwrap();
}

fn run(directory: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("CLI did not finish within eight seconds: {output:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
}

fn assert_arithmetic(report: &Value) {
    let metrics = &report["metrics"];
    let attempted = metrics["attempted"].as_u64().unwrap();
    let succeeded = metrics["succeeded"].as_u64().unwrap();
    let unexpected = metrics["unexpected_status"].as_u64().unwrap();
    let timeouts = metrics["timeouts"].as_u64().unwrap();
    let transport = metrics["transport_errors"].as_u64().unwrap();
    let complete = metrics["complete_responses"].as_u64().unwrap();
    assert_eq!(attempted, succeeded + unexpected + timeouts + transport);
    assert_eq!(metrics["latency"]["samples"], attempted);
    assert_eq!(complete, succeeded + unexpected);
    assert_eq!(
        metrics["status_counts"]
            .as_object()
            .unwrap()
            .values()
            .map(|value| value.as_u64().unwrap())
            .sum::<u64>(),
        complete
    );
    let elapsed = metrics["elapsed_secs"].as_f64().unwrap();
    assert!(elapsed > 0.0 && elapsed.is_finite());
    for (field, count) in [
        ("complete_responses_per_sec", complete),
        ("successful_responses_per_sec", succeeded),
    ] {
        let expected = count as f64 / elapsed;
        let actual = metrics[field].as_f64().unwrap();
        assert!(
            (actual - expected).abs() <= expected.max(1.0) * 1e-12,
            "{field} must use unrounded elapsed time: {actual} versus {expected}"
        );
    }
    let latency = &metrics["latency"];
    let p50 = latency["p50_us"].as_u64().unwrap();
    let p95 = latency["p95_us"].as_u64().unwrap();
    let p99 = latency["p99_us"].as_u64().unwrap();
    assert!(p50 > 0 && p50 <= p95 && p95 <= p99);
    assert!(
        report["finished_at_unix_ms"].as_u64().unwrap()
            >= report["started_at_unix_ms"].as_u64().unwrap()
    );
}

#[test]
fn post_report_preserves_config_metrics_and_build_metadata_without_request_secrets() {
    let directory = TempDir::new();
    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(20));
        respond(stream, 201);
    });
    let authority = server.url.strip_prefix("http://").unwrap();
    let url = format!(
        "http://report-user:report-password@{authority}/submit?token=query-secret#fragment-secret"
    );
    let sanitized_url = format!("{}/submit", server.url);
    let body = r#"{"private":"body-secret-☃"}"#;
    let output = run(
        &directory.0,
        &[
            "--output",
            "result.json",
            "post",
            &url,
            "--json",
            body,
            "--requests",
            "3",
            "--concurrency",
            "8",
            "--timeout-ms",
            "1500",
            "--expect-status",
            "201",
        ],
        "",
    );
    let requests = server.finish();
    assert_exit(&output, 0);
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.method == "POST"
        && request.path == "/submit?token=query-secret"
        && request.body == body.as_bytes()));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Succeeded: 3"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("JSON report: result.json"));
    assert_eq!(directory.files(), ["result.json"]);

    let document = directory.report();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["kind"], "request");
    assert_eq!(document["passed"], true);
    assert!(document["profile"].is_null());
    assert_eq!(document["runs"].as_array().unwrap().len(), 1);
    let run = &document["runs"][0];
    assert_eq!(run["passed"], true);
    assert!(run["route"].is_null() && run["error"].is_null());
    let report = &run["report"];
    assert_eq!(
        report["request"],
        json!({
            "method": "POST", "url": sanitized_url,
            "query_redacted": true, "body_bytes": body.len(),
        })
    );
    assert_eq!(
        report["workload"],
        json!({
            "mode": "requests", "requests": 3, "duration_secs": null,
            "requested_concurrency": 8, "effective_concurrency": 3,
            "timeout_ms": 1500, "expect_status": 201,
        })
    );
    assert_eq!(report["metrics"]["attempted"], 3);
    assert_eq!(report["metrics"]["succeeded"], 3);
    assert_eq!(report["metrics"]["status_counts"], json!({"201": 3}));
    assert_arithmetic(report);

    let runner = &document["runner"];
    assert_eq!(runner["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(runner["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        runner["rustc_version"]
            .as_str()
            .unwrap()
            .starts_with("rustc ")
    );
    assert!(!runner["target"].as_str().unwrap().is_empty());
    assert_eq!(runner["build_profile"], env!("RUSTA_BUILD_PROFILE"));
    // Source archives may have no Git metadata.
    if let Some(revision) = runner["git_revision"].as_str() {
        assert!(revision.len() >= 7 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(runner["source_dirty"].is_boolean());
    } else {
        assert!(runner["git_revision"].is_null());
    }
    assert_eq!(
        document["measurement"],
        json!({
            "protocol": "HTTP/1.1", "load_model": "closed_loop",
            "warmup": "none", "latency_scope": "all_attempts_through_full_body_or_failure",
            "redirects": false, "retries": false, "environment_proxies": false,
        })
    );
    let saved = fs::read_to_string(directory.0.join("result.json")).unwrap();
    for secret in [
        "report-user",
        "report-password",
        "query-secret",
        "fragment-secret",
        "body-secret",
    ] {
        assert!(!saved.contains(secret), "report leaked {secret}");
    }
}

#[test]
fn profile_reports_preserve_all_routes_and_respect_expected_failure_statuses() {
    for expected_failure in [false, true] {
        let directory = TempDir::new();
        let server = Server::start(move |request, stream| {
            let status = if expected_failure {
                503
            } else {
                match request.path.as_str() {
                    "/sleep/tokio-spawn" => 503,
                    "/sleep/park" => 429,
                    _ => 200,
                }
            };
            respond(stream, status);
        });
        let mut args = vec![
            "profile",
            "sleepapi",
            "--base-url",
            &server.url,
            "--requests",
            "2",
            "--concurrency",
            "1",
            "--delay-ms",
            "0",
            "--output",
            "result.json",
        ];
        if expected_failure {
            args.extend(["--scenario", "delayed_failure"]);
        }
        let output = run(&directory.0, &args, "");
        assert_eq!(server.finish().len(), 8);
        assert_exit(&output, i32::from(!expected_failure));
        let document = directory.report();
        assert_eq!(document["kind"], "profile");
        assert_eq!(document["passed"], expected_failure);
        assert_eq!(
            document["profile"],
            json!({
                "name": "sleepapi",
                "scenario": if expected_failure { Some("delayed_failure") } else { None },
                "delay_ms": 0, "status_code": if expected_failure { 503 } else { 200 },
            })
        );
        let runs = document["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 4);
        for (run, (route, mixed_status)) in runs.iter().zip([
            ("/sleep/tokio", 200),
            ("/sleep/tokio-spawn", 503),
            ("/sleep/thread", 200),
            ("/sleep/park", 429),
        ]) {
            let status = if expected_failure { 503 } else { mixed_status };
            let passed = expected_failure || status == 200;
            assert_eq!(run["route"], route);
            assert_eq!(run["passed"], passed);
            assert!(run["error"].is_null());
            let report = &run["report"];
            assert_eq!(report["metrics"]["attempted"], 2);
            assert_eq!(report["metrics"]["succeeded"], if passed { 2 } else { 0 });
            assert_eq!(
                report["metrics"]["unexpected_status"],
                if passed { 0 } else { 2 }
            );
            assert_eq!(
                report["metrics"]["status_counts"],
                json!({status.to_string(): 2})
            );
            assert_eq!(
                report["workload"]["expect_status"],
                if expected_failure { 503 } else { 200 }
            );
            assert_arithmetic(report);
        }
        assert_eq!(directory.files(), ["result.json"]);
    }
}

#[test]
fn duration_target_report_preserves_the_window_and_target_status_default() {
    let directory = TempDir::new();
    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(25));
        respond(stream, 401);
    });
    let output = run(
        &directory.0,
        &[
            "target",
            "roots-session-signed-out",
            "--base-url",
            &server.url,
            "--duration-secs",
            "1",
            "--concurrency",
            "2",
            "--output",
            "result.json",
        ],
        "",
    );
    let requests = server.finish();
    assert_exit(&output, 0);
    assert!(requests.len() > 2);
    assert!(
        requests
            .iter()
            .all(|request| request.path == "/api/auth/me")
    );
    let document = directory.report();
    assert_eq!(document["passed"], true);
    let report = &document["runs"][0]["report"];
    assert_eq!(report["request"]["method"], "GET");
    assert_eq!(report["request"]["body_bytes"], 0);
    assert_eq!(report["request"]["query_redacted"], false);
    assert_eq!(
        report["workload"],
        json!({
            "mode": "duration", "requests": null, "duration_secs": 1,
            "requested_concurrency": 2, "effective_concurrency": 2,
            "timeout_ms": 5000, "expect_status": 401,
        })
    );
    assert_eq!(report["metrics"]["attempted"], requests.len());
    assert_eq!(report["metrics"]["succeeded"], requests.len());
    assert!(report["metrics"]["elapsed_secs"].as_f64().unwrap() >= 1.0);
    assert_arithmetic(report);
}

#[test]
fn invalid_output_destinations_and_non_benchmark_commands_fail_without_traffic() {
    let directory = TempDir::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    fs::write(directory.0.join("existing.json"), "sentinel").unwrap();
    fs::create_dir(directory.0.join("existing-directory")).unwrap();
    let mut cases = [
        "existing.json",
        "existing-directory",
        "missing-parent/result.json",
        "existing.json/result.json",
        "new-report/",
        "new-report/.",
    ]
    .map(|destination| {
        (
            vec!["get", url.as_str(), "--output", destination],
            1,
            "Cannot prepare JSON report:",
        )
    })
    .to_vec();
    cases.extend([
        (
            vec!["catalogue", "--output", "result.json"],
            2,
            "--output requires a benchmark",
        ),
        (
            vec![
                "profile",
                "sleepapi",
                "--base-url",
                &url,
                "--dry-run",
                "--output",
                "result.json",
            ],
            2,
            "--output requires a benchmark",
        ),
    ]);
    for (args, code, error) in cases {
        let output = run(&directory.0, &args, "");
        assert_exit(&output, code);
        assert!(String::from_utf8_lossy(&output.stderr).contains(error));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    assert_eq!(
        fs::read_to_string(directory.0.join("existing.json")).unwrap(),
        "sentinel"
    );
    assert_eq!(directory.files(), ["existing-directory", "existing.json"]);
}

#[cfg(unix)]
#[test]
fn nonwritable_output_parent_fails_before_sending_requests() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new();
    let blocked = directory.0.join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
    // A privileged test process can bypass directory write permissions.
    if fs::write(blocked.join("probe"), "").is_ok() {
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let output = run(
        &directory.0,
        &["get", &url, "--output", "blocked/result.json"],
        "",
    );
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_exit(&output, 1);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read_dir(blocked).unwrap().count(), 0);
}

#[test]
fn wizard_only_creates_the_report_after_confirmation() {
    let directory = TempDir::new();
    let server = Server::start(|_, stream| respond(stream, 200));
    for confirmation in ["n\n", "q\n", ""] {
        let input = format!("3\n{}\n\n\n\n{confirmation}", server.url);
        let output = run(&directory.0, &["wizard", "--output", "result.json"], &input);
        assert_exit(&output, 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains("Cancelled."));
        assert!(directory.files().is_empty());
    }
    let input = format!("3\n{}\n\n\n\ny\n", server.url);
    let output = run(&directory.0, &["wizard", "--output", "result.json"], &input);
    assert_eq!(server.finish().len(), 1);
    assert_exit(&output, 0);
    assert_eq!(directory.report()["passed"], true);
    assert_eq!(directory.files(), ["result.json"]);
}

#[test]
fn benchmarks_without_output_keep_the_working_directory_empty() {
    let directory = TempDir::new();
    let server = Server::start(|_, stream| respond(stream, 200));
    let output = run(&directory.0, &["get", &server.url], "");
    assert_eq!(server.finish().len(), 1);
    assert_exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Succeeded: 1"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("JSON report:"));
    assert!(directory.files().is_empty());
}

#[test]
fn report_publication_preserves_a_destination_created_during_the_request() {
    let directory = TempDir::new();
    let destination = directory.0.join("result.json");
    let server = Server::start(move |_, stream| {
        fs::write(&destination, "created during the request").unwrap();
        respond(stream, 200);
    });
    let output = run(
        &directory.0,
        &["get", &server.url, "--output", "result.json"],
        "",
    );
    assert_eq!(server.finish().len(), 1);
    assert_exit(&output, 1);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Succeeded: 1"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cannot save JSON report:"));
    assert_eq!(
        fs::read_to_string(directory.0.join("result.json")).unwrap(),
        "created during the request"
    );
    assert_eq!(
        directory.files(),
        ["result.json"],
        "temporary reports must be cleaned up"
    );
}
