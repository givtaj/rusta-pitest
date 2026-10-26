use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    content_type: String,
    body: Value,
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Vec<Request>>,
}

impl Server {
    fn start(handler: impl Fn(&Request, &mut TcpStream) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut workers = Vec::new();
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let handler = Arc::clone(&handler);
                        workers.push(thread::spawn(move || {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let request = read_request(&mut stream);
                            handler(&request, &mut stream);
                            request
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("test server accept failed: {error}"),
                }
            }
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        Self { url, stop, worker }
    }

    fn finish(self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.join().unwrap()
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut start = line.split_whitespace();
    let method = start.next().unwrap().to_owned();
    let path = start.next().unwrap().to_owned();
    let mut content_type = String::new();
    let mut length = 0;
    loop {
        line.clear();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "incomplete headers"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-type" => content_type = value.trim().to_owned(),
            "content-length" => length = value.trim().parse::<usize>().unwrap(),
            _ => (),
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        content_type,
        body: serde_json::from_slice(&body).unwrap(),
    }
}

fn respond(stream: &mut TcpStream, status: u16) {
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
    .unwrap();
}

fn run(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "profile CLI timed out: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn assert_exit(output: &Output, code: i32) -> String {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn assert_requests(requests: &[Request], count_per_route: usize, body: Value) {
    assert_eq!(requests.len(), count_per_route * ROUTES.len());
    for (chunk, route) in requests.chunks(count_per_route).zip(ROUTES) {
        for request in chunk {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, route);
            assert_eq!(request.content_type, "application/json");
            assert_eq!(request.body, body);
        }
    }
}

#[test]
fn profile_sends_the_same_workload_to_four_endpoints_sequentially() {
    let state = Arc::new(Mutex::new((String::new(), 0_u32, 0_u32)));
    let server_state = Arc::clone(&state);
    let server = Server::start(move |request, stream| {
        {
            let mut state = server_state.lock().unwrap();
            if state.0 != request.path {
                assert_eq!(state.1, 0, "endpoints must not overlap");
                state.0 = request.path.clone();
            }
            state.1 += 1;
            state.2 = state.2.max(state.1);
        }
        thread::sleep(Duration::from_millis(30));
        server_state.lock().unwrap().1 -= 1;
        respond(stream, 200);
    });
    let output = run(&[
        "profile",
        "sleepapi",
        "--base-url",
        &server.url,
        "-n",
        "5",
        "-c",
        "2",
    ]);
    let requests = server.finish();
    let stdout = assert_exit(&output, 0);
    assert_requests(
        &requests,
        5,
        json!({"duration_ms": 400, "status_code": 200}),
    );
    assert_eq!(
        state.lock().unwrap().2,
        2,
        "respect per-endpoint concurrency"
    );
    assert_eq!(stdout.matches("Requests: 5\n").count(), 4);
    assert!(stdout.contains("Endpoints passed: 4/4"));
}

#[test]
fn named_failure_scenario_passes_with_its_expected_status_and_delay_override() {
    let server = Server::start(|_, stream| respond(stream, 503));
    let output = run(&[
        "profile",
        "sleepapi",
        "--base-url",
        &server.url,
        "-n",
        "1",
        "--scenario",
        "delayed_failure",
        "--delay-ms",
        "0",
    ]);
    let requests = server.finish();
    let stdout = assert_exit(&output, 0);
    assert_requests(
        &requests,
        1,
        json!({"scenario": "delayed_failure", "duration_ms": 0}),
    );
    assert!(stdout.contains("Expected status: 503"));
    assert_eq!(stdout.matches("Succeeded: 1\n").count(), 4);
}

#[test]
fn explicit_expected_status_overrides_scenario_and_response_configuration() {
    for options in [
        vec!["--scenario", "delayed_failure"],
        vec!["--status-code", "429"],
    ] {
        let server = Server::start(|_, stream| respond(stream, 201));
        let mut args = vec![
            "profile",
            "sleepapi",
            "--base-url",
            &server.url,
            "-n",
            "1",
            "--expect-status",
            "201",
        ];
        args.extend(options.iter().copied());
        let output = run(&args);
        let requests = server.finish();
        let stdout = assert_exit(&output, 0);
        let body = if options[0] == "--scenario" {
            json!({"scenario": "delayed_failure"})
        } else {
            json!({"duration_ms": 400, "status_code": 429})
        };
        assert_requests(&requests, 1, body);
        assert!(stdout.contains("Expected status: 201"));
    }
}

#[test]
fn one_failed_endpoint_does_not_prevent_the_remaining_endpoints_running() {
    let server = Server::start(|request, stream| {
        respond(stream, if request.path == ROUTES[0] { 500 } else { 200 });
    });
    let output = run(&["profile", "sleepapi", "--base-url", &server.url, "-n", "1"]);
    let requests = server.finish();
    let stdout = assert_exit(&output, 1);
    assert_requests(
        &requests,
        1,
        json!({"duration_ms": 400, "status_code": 200}),
    );
    assert!(stdout.contains("FAIL /sleep/tokio\n"));
    assert!(stdout.contains("PASS /sleep/park\n"));
    assert!(stdout.contains("Endpoints passed: 3/4"));
}

#[test]
fn client_timeout_scenario_keeps_timeouts_as_failures_and_continues() {
    let server = Server::start(|_, stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
            .unwrap();
        thread::sleep(Duration::from_millis(150));
        let _ = stream.write_all(b"{}");
    });
    let output = run(&[
        "profile",
        "sleepapi",
        "--base-url",
        &server.url,
        "-n",
        "1",
        "--scenario",
        "client_timeout",
        "--timeout-ms",
        "50",
    ]);
    let requests = server.finish();
    let stdout = assert_exit(&output, 1);
    assert_requests(&requests, 1, json!({"scenario": "client_timeout"}));
    assert_eq!(stdout.matches("Timeouts: 1\n").count(), 4);
    assert!(stdout.contains("Endpoints passed: 0/4"));
}

#[test]
fn dry_run_documents_defaults_and_all_scenarios_without_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let output = run(&["profile", "sleepapi", "--base-url", &url, "--dry-run"]);
    let stdout = assert_exit(&output, 0);
    for text in [
        "Requests per endpoint: 100",
        "Total planned requests: 400",
        "Concurrency per endpoint: 4",
        "Timeout per request: 5000 ms",
        "Expected status: 200",
        "Dry run: no requests sent.",
    ] {
        assert!(stdout.contains(text), "missing {text}: {stdout}");
    }
    for route in ROUTES {
        assert!(stdout.contains(&format!("POST {url}{route}")));
    }
    for (scenario, delay, status) in [
        ("fast_success", 50, 200),
        ("slow_provider", 2000, 200),
        ("delayed_failure", 800, 503),
        ("client_timeout", 3000, 200),
        ("rate_limited", 100, 429),
        ("invalid_request", 50, 400),
        ("upstream_timeout", 2000, 504),
    ] {
        let output = run(&[
            "profile",
            "sleepapi",
            "--base-url",
            &url,
            "--scenario",
            scenario,
            "--dry-run",
        ]);
        let stdout = assert_exit(&output, 0);
        assert!(stdout.contains(&format!("JSON: {{\"scenario\":\"{scenario}\"}}")));
        assert!(stdout.contains(&format!(
            "Server delay: {delay} ms; configured response status: {status}"
        )));
        assert!(stdout.contains(&format!("Expected status: {status}")));
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn invalid_profile_configuration_is_rejected_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for extra in [
        vec!["--requests", "0"],
        vec!["--duration-secs", "0"],
        vec!["--requests", "1", "--duration-secs", "1"],
        vec!["--concurrency", "0"],
        vec!["--timeout-ms", "0"],
        vec!["--scenario", "unknown"],
        vec!["--scenario", "fast_success", "--status-code", "200"],
        vec!["--delay-ms", "3600001"],
        vec!["--status-code", "199"],
        vec!["--status-code", "204"],
        vec!["--status-code", "205"],
        vec!["--status-code", "304"],
        vec!["--status-code", "600"],
        vec!["--expect-status", "600"],
    ] {
        let mut args = vec!["profile", "sleepapi", "--base-url", &url];
        args.extend(extra);
        let output = run(&args);
        assert_exit(&output, 2);
        assert!(!output.stderr.is_empty());
    }
    for origin in [
        format!("{url}/prefix"),
        format!("{url}?query=1"),
        format!("{url}#fragment"),
        format!("http://user:secret@{}", listener.local_addr().unwrap()),
    ] {
        let output = run(&["profile", "sleepapi", "--base-url", &origin]);
        assert_exit(&output, 2);
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn duration_applies_independently_to_every_endpoint() {
    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(100));
        respond(stream, 200);
    });
    let started = Instant::now();
    let output = run(&[
        "profile",
        "sleepapi",
        "--base-url",
        &server.url,
        "--duration-secs",
        "1",
        "--concurrency",
        "1",
    ]);
    let elapsed = started.elapsed();
    let requests = server.finish();
    let stdout = assert_exit(&output, 0);
    assert_eq!(stdout.matches("Request window: 1 s\n").count(), 4);
    assert!(
        elapsed >= Duration::from_secs(4),
        "each endpoint needs its own request window"
    );
    let mut remaining = requests.as_slice();
    for route in ROUTES {
        let count = remaining
            .iter()
            .take_while(|request| request.path == route)
            .count();
        assert!(
            count >= 2,
            "duration must allow several requests for {route}"
        );
        remaining = &remaining[count..];
    }
    assert!(remaining.is_empty(), "endpoints must execute in order");
}
