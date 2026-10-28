use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-workload-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        // create is atomic and refuses pre-existing paths; private permissions
        // protect intentional fixture rewrites inside this owned directory.
        directory.create(&path).unwrap();
        Self(path)
    }
    fn plan(&self, value: &Value) -> PathBuf {
        let path = self.0.join("plan.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let mut children = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let recorded = recorded.clone();
                        children.push(thread::spawn(move || serve(stream, recorded)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
            for child in children {
                child.join().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn seen(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn serve(mut stream: TcpStream, requests: Arc<Mutex<Vec<Request>>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().to_owned();
    let path = parts.next().unwrap().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    requests.lock().unwrap().push(Request {
        method: method.clone(),
        path: path.clone(),
        headers,
        body,
    });
    let status = if path.ends_with("/denied") {
        401
    } else if path.ends_with("/failure") {
        503
    } else {
        200
    };
    let body = br#"{"ok":true}"#;
    if write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).is_err() { return; }
    let _ = stream.flush();
    if path.ends_with("/slow") || path.ends_with("/write") {
        thread::sleep(Duration::from_millis(250));
    }
    if method != "HEAD" {
        let _ = stream.write_all(body);
    }
}

fn plan() -> Value {
    json!({"workload_version":1,"actors":{"a":{}},
        "requests":{"read":{"method":"GET","path":"/ok"}},
        "groups":{"load":{"actors":["a"],"max_in_flight":1,"requests":["read"]}},
        "phases":[{"name":"normal","duration_ms":100,"rates":{"load":10}}],
        "max_in_flight":1,"max_start_lag_ms":1000,"timeout_ms":1000,"drain_timeout_ms":1000})
}
fn command(temp: &Temp, plan: &Value, server: Option<&Server>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"));
    command
        .current_dir(&temp.0)
        .arg("workload")
        .arg("run")
        .arg(temp.plan(plan));
    if let Some(server) = server {
        command.arg("--execute").arg("--base-url").arg(&server.url);
        for id in plan["requests"].as_object().unwrap().keys() {
            command.arg("--operation").arg(id);
        }
    }
    command
}
fn report(output: Output, success: bool) -> Value {
    assert_eq!(
        output.status.success(),
        success,
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON report")
}
fn reconcile(report: &Value) {
    let c = &report["counts"];
    let n = |key: &str| c[key].as_u64().unwrap();
    assert_eq!(n("offered"), n("started") + n("unsent"));
    assert_eq!(n("started"), n("completed") + n("cancelled"));
    assert_eq!(n("completed"), n("passed") + n("failed"));
    assert_eq!(n("in_flight"), 0);
}

#[test]
fn preview_reads_no_credentials_or_body_files_and_reviews_writes() {
    let temp = Temp::new();
    let mut p = plan();
    p["actors"]["a"] = json!({"headers_env":{"Cookie":"RUSTA_TEST_ABSENT_COOKIE"}});
    p["requests"]["read"] =
        json!({"method":"POST","path":"/private-path","body_file":"not-present.bin"});
    let output = command(&temp, &p, None)
        .env_remove("RUSTA_TEST_ABSENT_COOKIE")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-path"));
    let r = report(output, true);
    assert_eq!(r["executed"], false);
    assert_eq!(r["runtime_secrets_loaded"], false);
    assert_eq!(r["requires_write_opt_in"], true);
}

#[test]
fn actors_are_isolated_expected_denials_pass_and_secrets_are_redacted() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["actors"] = json!({"a":{"headers_env":{"Cookie":"RUSTA_COOKIE_A","X-CSRF":"RUSTA_CSRF_A"}},"b":{"headers_env":{"Cookie":"RUSTA_COOKIE_B","X-CSRF":"RUSTA_CSRF_B"}}});
    p["requests"] = json!({"read":{"method":"GET","path":"/ok","response_schema":{"type":"object","required":["ok"],"properties":{"ok":{"const":true}}}},"denied":{"method":"GET","path":"/denied","expect_status":401}});
    p["groups"]["load"] =
        json!({"actors":["a","b"],"max_in_flight":2,"requests":["read","denied"]});
    p["max_in_flight"] = json!(2);
    p["phases"][0]["duration_ms"] = json!(400);
    let output = command(&temp, &p, Some(&server))
        .env("RUSTA_COOKIE_A", "session=actor-A-secret")
        .env("RUSTA_COOKIE_B", "session=actor-B-secret")
        .env("RUSTA_CSRF_A", "csrf-A-secret")
        .env("RUSTA_CSRF_B", "csrf-B-secret")
        .output()
        .unwrap();
    for secret in [
        "actor-A-secret",
        "actor-B-secret",
        "csrf-A-secret",
        "csrf-B-secret",
    ] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    let r = report(output, true);
    reconcile(&r);
    assert_eq!(r["counts"]["offered"], 4);
    assert_eq!(r["counts"]["passed"], 4);
    let seen = server.seen();
    assert_eq!(seen.len(), 4);
    for (index, request) in seen.iter().enumerate() {
        let actor = if index % 2 == 0 { "A" } else { "B" };
        assert_eq!(
            request.headers["cookie"],
            format!("session=actor-{actor}-secret")
        );
        assert_eq!(request.headers["x-csrf"], format!("csrf-{actor}-secret"));
    }
}

#[test]
fn whole_plan_preflight_and_output_checks_prevent_all_traffic() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["actors"]["a"] = json!({"headers_env":{"Cookie":"RUSTA_TEST_MISSING_COOKIE"}});
    p["setup"] = json!([{"actor":"a","request":"read"}]);
    let output = command(&temp, &p, Some(&server))
        .env_remove("RUSTA_TEST_MISSING_COOKIE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(server.seen().is_empty());
    p["actors"]["a"] = json!({});
    fs::write(temp.0.join("exists.json"), b"keep me").unwrap();
    let output = command(&temp, &p, Some(&server))
        .args(["--output", "exists.json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(server.seen().is_empty());
    assert_eq!(fs::read(temp.0.join("exists.json")).unwrap(), b"keep me");
    p["requests"]["read"]["method"] = json!("POST");
    let output = command(&temp, &p, Some(&server)).output().unwrap();
    assert!(!output.status.success());
    assert!(server.seen().is_empty());
}

#[test]
fn deliberate_write_replay_preserves_bytes_key_and_drains_admitted_sequence() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    let bytes = b"{ \"client\": \"synthetic-secret\", \"n\": 1 }\n";
    fs::write(temp.0.join("body.json"), bytes).unwrap();
    p["requests"] = json!({"write":{"method":"POST","path":"/write","body_file":"body.json","content_type":"application/json","idempotency_key_env":"RUSTA_TEST_KEY"},"setup":{"method":"GET","path":"/setup"}});
    p["setup"] = json!([{"actor":"a","request":"setup"}]);
    p["groups"]["load"] = json!({"actors":["a"],"max_in_flight":1,"sequence":[{"request":"write"},{"request":"write","replay_of":0}]});
    p["phases"][0]["rates"]["load"] = json!(1);
    let output = command(&temp, &p, Some(&server))
        .arg("--allow-writes")
        .env("RUSTA_TEST_KEY", "synthetic-idempotency-secret")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-idempotency-secret"));
    let r = report(output, true);
    reconcile(&r);
    assert_eq!(r["counts"]["offered"], 2);
    assert_eq!(r["counts"]["passed"], 2);
    assert_eq!(r["setup"]["measured"], false);
    assert_eq!(r["setup"]["results"].as_array().unwrap().len(), 1);
    assert_eq!(r["sequence_counts"]["passed"], 1);
    let seen = server.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].path, "/setup");
    for request in &seen[1..] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, bytes);
        assert_eq!(
            request.headers["idempotency-key"],
            "synthetic-idempotency-secret"
        );
    }
}

#[test]
fn relative_plan_filename_resolves_body_beside_plan() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    fs::write(temp.0.join("payload.bin"), b"exact body\n").unwrap();
    p["requests"]["read"] = json!({"method":"POST","path":"/ok","body_file":"payload.bin"});
    temp.plan(&p);
    let output = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .current_dir(&temp.0)
        .args([
            "workload",
            "run",
            "plan.json",
            "--execute",
            "--base-url",
            &server.url,
            "--operation",
            "read",
            "--allow-writes",
        ])
        .output()
        .unwrap();
    let r = report(output, true);
    reconcile(&r);
    assert_eq!(server.seen()[0].body, b"exact body\n");
}

#[test]
fn saturated_bulk_does_not_steal_canary_capacity_and_capture_loss_is_visible() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["actors"] = json!({"a":{},"b":{}});
    p["requests"] =
        json!({"bulk":{"method":"GET","path":"/slow"},"canary":{"method":"GET","path":"/ok"}});
    p["groups"] = json!({"bulk":{"actors":["a"],"max_in_flight":1,"requests":["bulk"]},"canary":{"actors":["b"],"max_in_flight":1,"requests":["canary"]}});
    p["max_in_flight"] = json!(2);
    p["phases"] = json!([{"name":"burst","duration_ms":100,"rates":{"bulk":100,"canary":10}}]);
    p["thresholds"] = json!([{"metric":"unsent_rate","maximum":1.0}]);
    p["evidence"] = json!({"max_records":2,"max_bytes":2048});
    let r = report(command(&temp, &p, Some(&server)).output().unwrap(), true);
    reconcile(&r);
    assert_eq!(r["counts"]["offered"], 11);
    assert_eq!(r["counts"]["started"], 2);
    assert_eq!(r["counts"]["unsent"], 9);
    assert_eq!(server.seen().iter().filter(|r| r.path == "/ok").count(), 1);
    assert_eq!(r["evidence"]["complete"], false);
    assert_eq!(r["evidence"]["captured_records"], 2);
    assert_eq!(r["evidence"]["dropped_records"], 9);
    let rows = r["series"].as_array().unwrap();
    assert_eq!(
        rows.iter().find(|row| row["group"] == "canary").unwrap()["metrics"]["counts"]["passed"],
        1
    );
}

#[test]
fn phase_attribution_and_threshold_exit_status_are_explicit() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["phases"] = json!(
        ["normal", "sustained", "burst", "recovery"]
            .map(|name| json!({"name":name,"duration_ms":100,"rates":{"load":10}}))
    );
    p["thresholds"] = json!([{"metric":"max_ms","maximum":0.0}]);
    let r = report(command(&temp, &p, Some(&server)).output().unwrap(), false);
    reconcile(&r);
    assert_eq!(r["counts"]["passed"], 4);
    assert_eq!(r["series"].as_array().unwrap().len(), 4);
    assert_eq!(r["thresholds"][0]["reason"], "maximum_exceeded");
    p["thresholds"] = json!([{"metric":"error_rate","maximum":0.0,"min_samples":5}]);
    let r = report(command(&temp, &p, Some(&server)).output().unwrap(), false);
    assert_eq!(r["thresholds"][0]["reason"], "insufficient_samples");
}

#[test]
fn failed_sequence_stops_and_body_timeout_is_not_a_complete_http_response() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["requests"] =
        json!({"first":{"method":"GET","path":"/failure"},"later":{"method":"GET","path":"/ok"}});
    p["groups"]["load"] = json!({"actors":["a"],"max_in_flight":1,"sequence":[{"request":"first"},{"request":"later"}]});
    let r = report(command(&temp, &p, Some(&server)).output().unwrap(), false);
    reconcile(&r);
    assert_eq!(r["counts"]["failed"], 1);
    assert_eq!(r["counts"]["unsent"], 1);
    assert_eq!(server.seen().len(), 1);
    p = plan();
    p["requests"]["read"]["path"] = json!("/slow");
    p["timeout_ms"] = json!(30);
    let r = report(command(&temp, &p, Some(&server)).output().unwrap(), false);
    reconcile(&r);
    assert_eq!(r["counts"]["failed"], 1);
    assert_eq!(r["counts"]["complete_http_responses"], 0);
}

#[cfg(unix)]
#[test]
fn interruption_publishes_complete_accounting_and_incomplete_capture() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["requests"]["read"]["path"] = json!("/slow");
    p["phases"][0]["duration_ms"] = json!(60_000);
    p["drain_timeout_ms"] = json!(10);
    // Drain output into files while waiting: a full pipe must not block report publication.
    let out_path = temp.0.join("stdout.json");
    let err_path = temp.0.join("stderr.txt");
    let mut child = command(&temp, &p, Some(&server))
        .stdout(Stdio::from(fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&err_path).unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.seen().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(!server.seen().is_empty(), "first request must start");
    assert!(
        Command::new("kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("workload failed to stop after interrupt");
    }
    let r = report(
        Output {
            status: child.wait().unwrap(),
            stdout: fs::read(out_path).unwrap(),
            stderr: fs::read(err_path).unwrap(),
        },
        false,
    );
    reconcile(&r);
    assert_eq!(r["interrupted"], true);
    assert_eq!(r["counts"]["offered"], 600);
    assert_eq!(r["evidence"]["complete"], false);
    assert_eq!(r["counts"]["cancelled"], 1);
}
