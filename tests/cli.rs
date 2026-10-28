use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug)]
struct Request {
    connection_id: usize,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    worker: JoinHandle<Vec<Request>>,
}

impl Server {
    fn start(handler: impl Fn(&Request, &mut TcpStream) + Send + Sync + 'static) -> Self {
        Self::start_with_keep_alive(false, handler)
    }

    fn start_with_keep_alive(
        keep_alive: bool,
        handler: impl Fn(&Request, &mut TcpStream) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let connections = Arc::new(AtomicUsize::new(0));
        let server_connections = Arc::clone(&connections);
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut workers = Vec::new();
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets inherit nonblocking mode on macOS.
                        // Request parsing below intentionally uses blocking I/O.
                        stream.set_nonblocking(false).unwrap();
                        let connection_id = server_connections.fetch_add(1, Ordering::Relaxed);
                        let handler = Arc::clone(&handler);
                        workers.push(thread::spawn(move || {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut requests = Vec::new();
                            while let Some(request) = read_request(&mut stream, connection_id) {
                                handler(&request, &mut stream);
                                requests.push(request);
                                if !keep_alive {
                                    break;
                                }
                            }
                            requests
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
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        Self {
            url,
            stop,
            connections,
            worker,
        }
    }

    fn finish(self) -> Vec<Request> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.join().unwrap()
    }
}

fn read_request(stream: &mut TcpStream, connection_id: usize) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap() == 0 {
        return None;
    }
    let mut start = line.split_whitespace();
    let method = start.next().unwrap().to_owned();
    let path = start.next().unwrap().to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "incomplete request headers"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').unwrap();
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Some(Request {
        connection_id,
        method,
        path,
        headers,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn run(args: &[&str]) -> Output {
    run_with_input(args, "")
}

fn run_with_input(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .args(args)
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
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

fn assert_report(output: &Output, exit_code: i32, labels: &[&str]) {
    assert_eq!(output.status.code(), Some(exit_code), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for label in labels {
        assert!(
            stdout.lines().any(|line| line.trim() == *label),
            "missing {label:?} in {stdout:?}"
        );
    }
}

#[test]
fn concurrent_get_sends_exactly_the_requested_count() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server_active = Arc::clone(&active);
    let server_peak = Arc::clone(&peak);
    let server = Server::start(move |_, stream| {
        let current = server_active.fetch_add(1, Ordering::SeqCst) + 1;
        server_peak.fetch_max(current, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(25));
        server_active.fetch_sub(1, Ordering::SeqCst);
        respond(stream, "200 OK", b"ok");
    });
    let output = run(&[
        "get",
        &format!("{}/health", server.url),
        "--requests",
        "10",
        "--concurrency",
        "3",
    ]);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &[
            "Requests: 10",
            "Succeeded: 10",
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 0",
            "Latency samples: 10",
        ],
    );
    assert_eq!(requests.len(), 10);
    assert!(requests.iter().all(|request| request.method == "GET"
        && request.path == "/health"
        && request.body.is_empty()));
    assert!(
        (2..=3).contains(&peak.load(Ordering::SeqCst)),
        "configured concurrency was not respected"
    );
}

#[test]
fn sequential_requests_drain_chunked_bodies_and_reuse_connections() {
    let pending_body = Arc::new(AtomicBool::new(false));
    let bodies_finished = Arc::new(AtomicUsize::new(0));
    let server_pending = Arc::clone(&pending_body);
    let server_finished = Arc::clone(&bodies_finished);
    let server = Server::start_with_keep_alive(true, move |_, stream| {
        assert!(
            !server_pending.swap(true, Ordering::SeqCst),
            "the next sequential request started before the previous body was released"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n")
            .unwrap();
        // A response must remain in flight after headers and its first chunk;
        // dropping it early must not look like successful full-body timing.
        thread::sleep(Duration::from_millis(10));
        // Release the guard before writing EOF: a correct client can start its
        // next request as soon as those bytes arrive, before this thread resumes.
        server_pending.store(false, Ordering::SeqCst);
        stream.write_all(b"4\r\ndone\r\n0\r\n\r\n").unwrap();
        server_finished.fetch_add(1, Ordering::SeqCst);
    });
    let output = run(&["get", &server.url, "--requests", "16"]);
    let connections = Arc::clone(&server.connections);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &[
            "Requests: 16",
            "Succeeded: 16",
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 0",
            "HTTP statuses (complete responses): 200=16",
            "Latency samples: 16",
        ],
    );
    assert_eq!(requests.len(), 16);
    assert_eq!(bodies_finished.load(Ordering::SeqCst), 16);
    let mut requests_per_connection = std::collections::BTreeMap::new();
    for request in &requests {
        *requests_per_connection
            .entry(request.connection_id)
            .or_insert(0) += 1;
    }
    // Hyper returns HTTP/1 connections to its pool on a separate readiness task
    // and races checkout against connecting. Extra accepted sockets, including
    // unused ones, do not prove a drain/reuse failure. Observe actual reuse over
    // a bounded sample without requiring every request to use the same socket.
    assert!(
        requests_per_connection.values().any(|&count| count > 1),
        "no connection was reused: accepted={}, requests_per_connection={requests_per_connection:?}",
        connections.load(Ordering::Relaxed)
    );
}

#[test]
fn timed_get_keeps_concurrency_and_finishes_bodies_started_before_the_deadline() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server_active = Arc::clone(&active);
    let server_peak = Arc::clone(&peak);
    let server = Server::start(move |_, stream| {
        let current = server_active.fetch_add(1, Ordering::SeqCst) + 1;
        server_peak.fetch_max(current, Ordering::SeqCst);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
            .unwrap();
        // Two waves fit inside the one-second request window. The second
        // wave's bodies finish after it, and must still be counted.
        thread::sleep(Duration::from_millis(650));
        server_active.fetch_sub(1, Ordering::SeqCst);
        stream.write_all(b"done").unwrap();
    });
    let started = Instant::now();
    let output = run(&[
        "get",
        &format!("{}/health", server.url),
        "--duration-secs",
        "1",
        "--concurrency",
        "2",
    ]);
    let elapsed = started.elapsed();
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &[
            "Request window: 1 s",
            "Concurrency: 2",
            "Requests: 4",
            "Succeeded: 4",
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 0",
            "HTTP statuses (complete responses): 200=4",
            "Latency samples: 4",
        ],
    );
    assert_eq!(
        requests.len(),
        4,
        "no new wave may start after the deadline"
    );
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert!(
        elapsed >= Duration::from_millis(1300) && elapsed < Duration::from_secs(4),
        "timed run must include the request window and final response bodies: {elapsed:?}"
    );
    assert!(requests.iter().all(|request| request.method == "GET"
        && request.path == "/health"
        && request.body.is_empty()));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported_seconds = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Elapsed: "))
        .unwrap()
        .strip_suffix(" s")
        .unwrap()
        .parse::<f64>()
        .unwrap();
    let throughput = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Throughput: "))
        .unwrap()
        .strip_suffix(" complete responses/s")
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!(reported_seconds >= 1.3, "elapsed must include draining");
    assert!(
        (throughput - requests.len() as f64 / reported_seconds).abs() < 0.01,
        "throughput must use total elapsed time, including draining"
    );
}

#[test]
fn post_delivers_the_original_json_and_content_type() {
    let server = Server::start(|_, stream| respond(stream, "201 Created", b"{}"));
    let json = "{ \"name\": \"roots\", \"enabled\": true }";
    let output = run(&["post", &format!("{}/items", server.url), "--json", json]);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &["Requests: 1", "Succeeded: 1", "Latency samples: 1"],
    );
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/items");
    assert_eq!(request.body, json.as_bytes());
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/json")
    );
}

#[test]
fn redirects_are_not_followed_and_an_expected_error_status_can_succeed() {
    let server = Server::start(|request, stream| {
        if request.path == "/redirect" {
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        } else {
            respond(stream, "200 OK", b"followed");
        }
    });
    let output = run(&["get", &format!("{}/redirect", server.url)]);
    let requests = server.finish();
    assert_report(
        &output,
        1,
        &[
            "Requests: 1",
            "Succeeded: 0",
            "Unexpected status: 1",
            "Latency samples: 1",
        ],
    );
    assert_eq!(requests.len(), 1, "redirect must not cause another request");

    let server = Server::start(|_, stream| respond(stream, "404 Not Found", b"missing"));
    let output = run(&["get", &server.url, "--expect-status", "404"]);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &["Succeeded: 1", "Unexpected status: 0", "Latency samples: 1"],
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn timeout_while_reading_the_response_body_is_a_failed_attempt() {
    let server = Server::start(|_, stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let _ = stream.write_all(b"done");
    });
    let output = run(&["get", &server.url, "--timeout-ms", "100"]);
    let requests = server.finish();
    assert_report(
        &output,
        1,
        &[
            "Requests: 1",
            "Succeeded: 0",
            "Unexpected status: 0",
            "Timeouts: 1",
            "Transport errors: 0",
            "Latency samples: 1",
        ],
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn truncated_response_body_is_a_transport_error_without_a_completed_status() {
    let server = Server::start(|_, stream| {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nx")
            .unwrap();
    });
    let output = run(&["get", &server.url]);
    let requests = server.finish();
    assert_report(
        &output,
        1,
        &[
            "Requests: 1",
            "Succeeded: 0",
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 1",
            "HTTP statuses (complete responses): none",
            "Latency samples: 1",
        ],
    );
    assert_eq!(requests.len(), 1);
}

#[test]
fn refused_connections_count_as_failed_attempts_and_do_not_stop_the_run() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = run(&["get", &url, "--requests", "3", "--timeout-ms", "1000"]);
    assert_report(
        &output,
        1,
        &[
            "Requests: 3",
            "Succeeded: 0",
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 3",
            "HTTP statuses (complete responses): none",
            "Latency samples: 3",
        ],
    );
}

#[test]
fn mixed_outcomes_account_for_every_attempt_and_only_count_completed_statuses() {
    let response_number = AtomicUsize::new(0);
    let server =
        Server::start(
            move |_, stream| match response_number.fetch_add(1, Ordering::SeqCst) {
                0 => respond(stream, "503 Service Unavailable", b"unavailable"),
                1 => stream
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 4\r\nConnection: close\r\n\r\nx",
                    )
                    .unwrap(),
                2 => {
                    stream
                    .write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    thread::sleep(Duration::from_millis(1200));
                    let _ = stream.write_all(b"done");
                }
                3 => respond(stream, "200 OK", b"ok"),
                index => panic!("unexpected extra request {index}"),
            },
        );
    let output = run(&[
        "get",
        &server.url,
        "--requests",
        "4",
        "--concurrency",
        "2",
        "--timeout-ms",
        "500",
        "--expect-status",
        "200",
    ]);
    let requests = server.finish();
    assert_report(
        &output,
        1,
        &[
            "Requests: 4",
            "Succeeded: 1",
            "Unexpected status: 1",
            "Timeouts: 1",
            "Transport errors: 1",
            "HTTP statuses (complete responses): 200=1, 503=1",
            "Latency samples: 4",
        ],
    );
    assert_eq!(requests.len(), 4);
}

#[test]
fn invalid_configuration_is_rejected_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for args in [
        vec!["get", &url, "--requests", "0"],
        vec!["get", &url, "--duration-secs", "0"],
        vec!["get", &url, "--requests", "1", "--duration-secs", "1"],
        vec!["post", &url, "--json", "{invalid}"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            !output.stderr.is_empty(),
            "invalid configuration needs an explanation"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "invalid configuration must not open a connection"
        );
    }
}

#[test]
fn no_arguments_starts_a_wizard_that_retries_invalid_benchmark_input() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server_active = Arc::clone(&active);
    let server_peak = Arc::clone(&peak);
    let server = Server::start(move |_, stream| {
        let current = server_active.fetch_add(1, Ordering::SeqCst) + 1;
        server_peak.fetch_max(current, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(25));
        server_active.fetch_sub(1, Ordering::SeqCst);
        respond(stream, "200 OK", b"ok");
    });
    let input = format!("3\n{}/health\n2\nnot-a-number\n6\n2\n\n\n\n", server.url);
    let output = run_with_input(&[], &input);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &[
            "Concurrency: 2",
            "Requests: 6",
            "Succeeded: 6",
            "Unexpected status: 0",
            "Latency samples: 6",
        ],
    );
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| request.method == "GET"
        && request.path == "/health"
        && request.body.is_empty()));
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[test]
fn wizard_runs_a_timed_benchmark() {
    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(50));
        respond(stream, "200 OK", b"ok");
    });
    let input = format!("3\n{}/health\n3\n1\n2\n\n\n\n", server.url);
    let output = run_with_input(&["wizard"], &input);
    let requests = server.finish();
    let count = requests.len();
    assert!(count > 2, "timed mode must continue beyond the first wave");
    assert_report(
        &output,
        0,
        &[
            "Request window: 1 s",
            "Concurrency: 2",
            &format!("Requests: {count}"),
            &format!("Succeeded: {count}"),
            "Unexpected status: 0",
            "Timeouts: 0",
            "Transport errors: 0",
            &format!("Latency samples: {count}"),
        ],
    );
}

#[test]
fn wizard_post_retries_json_and_uses_the_exact_expected_status() {
    let response_number = AtomicUsize::new(0);
    let server = Server::start(move |_, stream| {
        let status = if response_number.fetch_add(1, Ordering::SeqCst) == 0 {
            "201 Created"
        } else {
            "202 Accepted"
        };
        respond(stream, status, b"{}");
    });
    let json = "{ \"name\": \"roots\", \"enabled\": true }";
    let input = format!("4\n{}/items\n{{invalid}}\n{json}\n\n\n201\ny\n", server.url);
    let success = run_with_input(&["wizard"], &input);
    let unexpected_status = run_with_input(&["wizard"], &input);
    let requests = server.finish();
    assert_report(
        &success,
        0,
        &[
            "Requests: 1",
            "Succeeded: 1",
            "Unexpected status: 0",
            "Latency samples: 1",
        ],
    );
    assert_report(
        &unexpected_status,
        1,
        &["Requests: 1", "Succeeded: 0", "Unexpected status: 1"],
    );
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/items");
        assert_eq!(request.body, json.as_bytes());
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "content-type" && value == "application/json")
        );
    }
}

#[test]
fn wizard_cancellation_and_eof_at_confirmation_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for (target, body) in [("3", ""), ("4", "\n")] {
        for confirmation in ["n\n", "q\n", "", " ", "\t", "\r", "y", "yes", " y "] {
            let input = format!("{target}\n{url}\n{body}\n100\n\n{confirmation}");
            let output = run_with_input(&["wizard"], &input);
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("Cancelled."),
                "cancellation needs an explanation: {output:?}"
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "unconfirmed wizard must not open a connection"
            );
        }
    }
}

#[test]
fn wizard_completed_blank_confirmation_accepts_defaults_for_lf_and_crlf() {
    for newline in ["\n", "\r\n"] {
        for target in ["3", "4", "5", "6"] {
            let server = Server::start(|_, stream| respond(stream, "200 OK", b"{}"));
            let input = match target {
                "3" => format!("3\n{}\n\n\n\n\n", server.url),
                "4" => format!("4\n{}\n\n\n\n\n\n", server.url),
                "5" => format!("5\n1\n{}\n\n\n\n\n", server.url),
                "6" => format!("6\n{}\nbaseline\n0\n\n1\n\n\n\n", server.url),
                _ => unreachable!(),
            }
            .replace('\n', newline);
            let output = run_with_input(&["wizard"], &input);
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(!String::from_utf8_lossy(&output.stdout).contains("Cancelled."));
            assert_eq!(server.finish().len(), if target == "6" { 4 } else { 1 });
        }
    }
}

#[test]
fn catalogue_and_alias_list_available_checks_without_running_requests() {
    for command in ["catalogue", "catalog"] {
        let output = run(&[command]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        for id in [
            "roots-hello",
            "roots-health",
            "roots-openapi",
            "roots-docs",
            "roots-session-signed-out",
            "roots-customer-signed-out",
            "roots-strains-signed-out",
            "roots-experiences-signed-out",
            "roots-calendar-signed-out",
        ] {
            assert!(stdout.contains(id), "missing {id:?} in {stdout:?}");
        }
        assert!(!stdout.contains("Latency samples:"), "{stdout:?}");
        assert!(!stdout.contains("Running…"), "{stdout:?}");
    }
}

#[test]
fn target_defaults_require_exact_status_and_allow_an_explicit_override() {
    for (id, path, status, override_status, exit_code) in [
        ("roots-hello", "/api/hello", "200 OK", None, 0),
        ("roots-hello", "/api/hello", "201 Created", None, 1),
        (
            "roots-session-signed-out",
            "/api/auth/me",
            "401 Unauthorized",
            None,
            0,
        ),
        (
            "roots-session-signed-out",
            "/api/auth/me",
            "200 OK",
            None,
            1,
        ),
        (
            "roots-session-signed-out",
            "/api/auth/me",
            "200 OK",
            Some("200"),
            0,
        ),
    ] {
        let server = Server::start(move |_, stream| respond(stream, status, b"{}"));
        let base_url = format!("{}/", server.url);
        let mut args = vec!["target", id, "--base-url", &base_url];
        if let Some(expected) = override_status {
            args.extend(["--expect-status", expected]);
        }
        let output = run(&args);
        let requests = server.finish();
        let succeeded = usize::from(exit_code == 0);
        assert_report(
            &output,
            exit_code,
            &[
                "Requests: 1",
                &format!("Succeeded: {succeeded}"),
                &format!("Unexpected status: {}", 1 - succeeded),
                "Latency samples: 1",
            ],
        );
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, path);
        assert!(request.body.is_empty());
        assert!(
            request
                .headers
                .iter()
                .all(|(name, _)| name != "authorization" && name != "cookie")
        );
    }
}

#[test]
fn target_count_and_duration_runs_preserve_query_parameters() {
    let server = Server::start(|_, stream| respond(stream, "401 Unauthorized", b"{}"));
    let output = run(&[
        "target",
        "roots-strains-signed-out",
        "--base-url",
        &server.url,
        "--requests",
        "3",
        "--concurrency",
        "2",
    ]);
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &["Concurrency: 2", "Requests: 3", "Succeeded: 3"],
    );
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.method == "GET"
        && request.path == "/api/strains?limit=1"
        && request.body.is_empty()));

    let server = Server::start(|_, stream| {
        thread::sleep(Duration::from_millis(50));
        respond(stream, "401 Unauthorized", b"{}");
    });
    let output = run(&[
        "target",
        "roots-calendar-signed-out",
        "--base-url",
        &server.url,
        "--duration-secs",
        "1",
        "--concurrency",
        "2",
    ]);
    let requests = server.finish();
    let count = requests.len();
    assert!(
        count > 2,
        "timed targets must continue beyond the first wave"
    );
    assert_report(
        &output,
        0,
        &[
            "Request window: 1 s",
            "Concurrency: 2",
            &format!("Requests: {count}"),
            &format!("Succeeded: {count}"),
            "Unexpected status: 0",
        ],
    );
    assert!(requests.iter().all(|request| request.method == "GET"
        && request.path == "/api/experiences/calendar?month=2026-09"
        && request.body.is_empty()));
}

#[test]
fn invalid_targets_and_base_urls_are_rejected_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let base_url = format!("http://{authority}");
    let invalid_origins = [
        format!("http://user:secret@{authority}"),
        format!("{base_url}/prefix"),
        format!("{base_url}?query=1"),
        format!("{base_url}#fragment"),
        format!("ftp://{authority}"),
    ];
    let mut cases = vec![
        vec!["target", "unknown-target", "--base-url", &base_url],
        vec![
            "target",
            "roots-hello",
            "--base-url",
            &base_url,
            "--requests",
            "0",
        ],
        vec![
            "target",
            "roots-hello",
            "--base-url",
            &base_url,
            "--duration-secs",
            "0",
        ],
        vec![
            "target",
            "roots-hello",
            "--base-url",
            &base_url,
            "--requests",
            "1",
            "--duration-secs",
            "1",
        ],
    ];
    for origin in &invalid_origins {
        cases.push(vec!["target", "roots-hello", "--base-url", origin]);
    }
    for args in cases {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "invalid target configuration must not open a connection"
        );
    }
}

#[test]
fn wizard_catalogue_retries_selection_and_defaults_to_signed_out_status() {
    let server = Server::start(|_, stream| respond(stream, "401 Unauthorized", b"{}"));
    for selection in ["6", "roots-customer-signed-out"] {
        let input = format!("5\nnot-a-target\n{selection}\n{}/\n\n\n\ny\n", server.url);
        let output = run_with_input(&["wizard"], &input);
        assert_report(
            &output,
            0,
            &[
                "Expected status: 401",
                "Requests: 1",
                "Succeeded: 1",
                "Unexpected status: 0",
            ],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("GET {}/api/customers/me", server.url)),
            "review must show the resolved target URL: {stdout:?}"
        );
    }
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.method == "GET"
            && request.path == "/api/customers/me"
            && request.body.is_empty()
            && request
                .headers
                .iter()
                .all(|(name, _)| name != "authorization" && name != "cookie")
    }));
}

#[test]
fn wizard_catalogue_cancellation_and_eof_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    for confirmation in ["n\n", "q\n", "", " ", "\t", "\r", "y", "yes", " y "] {
        let input = format!("5\nroots-session-signed-out\n{base_url}\n\n100\n\n{confirmation}");
        let output = run_with_input(&["wizard"], &input);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Cancelled."),
            "cancellation needs an explanation: {output:?}"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "unconfirmed catalogue selection must not open a connection"
        );
    }
}

#[test]
fn wizard_sleepapi_profile_runs_all_four_routes_with_scenario_defaults() {
    let server = Server::start(|_, stream| respond(stream, "503 Service Unavailable", b"{}"));
    let input = format!(
        "6\n{}\nnot-a-scenario\ndelayed_failure\n0\n2\n2\n2\n\n\ny\n",
        server.url
    );
    let output = run_with_input(&["wizard"], &input);
    let base_url = server.url.clone();
    let requests = server.finish();
    assert_report(
        &output,
        0,
        &["Expected status: 503", "Endpoints passed: 4/4"],
    );
    assert_eq!(requests.len(), 8);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for (index, path) in [
        "/sleep/tokio",
        "/sleep/tokio-spawn",
        "/sleep/thread",
        "/sleep/park",
    ]
    .iter()
    .enumerate()
    {
        assert!(stdout.contains(&format!("POST {base_url}{path}")));
        for request in &requests[index * 2..index * 2 + 2] {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, *path);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
                serde_json::json!({"scenario": "delayed_failure", "duration_ms": 0})
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "content-type" && value == "application/json")
            );
        }
    }
}

#[test]
fn wizard_sleepapi_cancellation_and_eof_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    for confirmation in ["n\n", "q\n", "", " ", "\t", "\r", "y", "yes", " y "] {
        let input = format!("6\n{base_url}\nbaseline\n\n\n1\n100\n\n{confirmation}");
        let output = run_with_input(&["wizard"], &input);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("Cancelled."));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "unconfirmed profile must not open a connection"
        );
    }
}
