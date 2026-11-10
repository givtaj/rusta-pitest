use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
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
        // Runtime TMPDIR cannot redirect synthetic fixture paths.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("workload-test-fixtures");
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.recursive(true).create(&root).unwrap();
        let metadata = fs::symlink_metadata(&root).unwrap();
        assert!(
            metadata.is_dir(),
            "test fixture root must be a real directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Refuse parents that let another user replace private fixtures.
            // Existing directories are checked, never chmodded by the tests.
            let target = fs::metadata(root.parent().unwrap()).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o022, 0);
            assert_eq!(target.permissions().mode() & 0o022, 0);
        }
        let root = root.canonicalize().unwrap();
        let path = root.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // Atomic creation refuses pre-existing paths, including symlinks.
        directory.recursive(false).create(&path).unwrap();
        let path = path.canonicalize().unwrap();
        if !path.starts_with(&root) || path.parent() != Some(root.as_path()) {
            panic!("test fixture directory escaped its root");
        }
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
    let route = path.split('?').next().unwrap();
    if route.ends_with("/disconnect") {
        return;
    }
    let status = if route.ends_with("/denied") {
        401
    } else if route.ends_with("/failure") {
        503
    } else {
        200
    };
    let body = br#"{"ok":true}"#;
    if write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).is_err() { return; }
    let _ = stream.flush();
    if route.ends_with("/slow") || route.ends_with("/write") {
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
fn expanded_response_schema_is_rejected_before_setup_or_measured_requests() {
    let temp = Temp::new();
    let server = Server::new();
    let mut definitions = serde_json::Map::new();
    definitions.insert("n0".into(), json!({"type":"object"}));
    for level in 1..=12 {
        let reference = format!("#/$defs/n{}", level - 1);
        definitions.insert(
            format!("n{level}"),
            json!({"allOf":[
                {"$ref":reference}, {"$ref":reference}
            ]}),
        );
    }
    let mut p = plan();
    p["requests"]["read"]["response_schema"] = json!({
        "$defs":definitions, "$ref":"#/$defs/n12"
    });
    p["setup"] = json!([{"actor":"a","request":"read"}]);
    for execute in [false, true] {
        let output = command(&temp, &p, execute.then_some(&server))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("response_schema_unsupported"));
        assert!(server.seen().is_empty());
    }
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
    p["requests"]["read"]["query"] = json!([{"name":"scope","value":"actor-private+scope"}]);
    p["requests"]["denied"]["query"] = json!([{"name":"scope","value":"actor-private+scope"}]);
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
        "actor-private+scope",
        "actor-private%2Bscope",
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
        assert!(request.path.ends_with("?scope=actor-private%2Bscope"));
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
    p["requests"]["write"]["query"] =
        json!([{"name":"intent","value":"same+intent"},{"name":"intent","value":""}]);
    p["requests"]["setup"]["query"] = json!([{"name":"phase","value":"setup only"}]);
    p["groups"]["load"] = json!({"actors":["a"],"max_in_flight":1,"sequence":[{"request":"write"},{"request":"write","replay_of":0}]});
    p["phases"][0]["rates"]["load"] = json!(1);
    let refused = command(&temp, &p, Some(&server))
        .env("RUSTA_TEST_KEY", "synthetic-idempotency-secret")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        server.seen().is_empty(),
        "write opt-in must precede even setup traffic"
    );
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
    assert_eq!(seen[0].path, "/setup?phase=setup%20only");
    for request in &seen[1..] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/write?intent=same%2Bintent&intent=");
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

fn plan_with_later_query(query: Value) -> Value {
    let mut p = plan();
    p["requests"]["z_later"] = json!({"method":"GET","path":"/later","query":query});
    p["setup"] = json!([{"actor":"a","request":"read"}]);
    p["groups"]["load"]["requests"] = json!(["read", "z_later"]);
    p
}

#[test]
fn query_pairs_preserve_wire_order_repeats_empty_values_and_encode_utf8_once() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["requests"]["read"]["query"] = json!([
        {"name":"tag","value":"first"},
        {"name":"tag","value":"second"},
        {"name":"empty","value":""},
        {"name":"na me+☃","value":"space +/%?#=&;☃\n\u{0}"},
        {"name":"unreserved-._~","value":"AZaz09-._~"},
        {"name":"literal","value":"${RUSTA_QUERY_LITERAL}"}
    ]);
    let path = temp.plan(&p);
    let output = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .current_dir(&temp.0)
        .args(["workload", "run"])
        .arg(path)
        .args([
            "--execute",
            "--base-url",
            &format!("{}/base/prefix/", server.url),
            "--operation",
            "read",
        ])
        .env("RUSTA_QUERY_LITERAL", "must-not-be-interpolated-private")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-be-interpolated-private"));
    let r = report(output, true);
    reconcile(&r);
    assert_eq!(r["schema_version"], 2);
    assert_eq!(r["counts"]["passed"], 1);
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].path,
        "/base/prefix/ok?tag=first&tag=second&empty=&na%20me%2B%E2%98%83=space%20%2B%2F%25%3F%23%3D%26%3B%E2%98%83%0A%00&unreserved-._~=AZaz09-._~&literal=%24%7BRUSTA_QUERY_LITERAL%7D"
    );
}

#[test]
fn omitted_or_empty_query_retains_the_original_request_target() {
    for query in [None, Some(json!([]))] {
        let temp = Temp::new();
        let server = Server::new();
        let mut p = plan();
        if let Some(query) = query {
            p["requests"]["read"]["query"] = query;
        }
        let r = report(command(&temp, &p, Some(&server)).output().unwrap(), true);
        reconcile(&r);
        assert_eq!(server.seen()[0].path, "/ok");
    }
}

#[test]
fn invalid_query_shapes_types_and_limits_fail_before_setup_or_measured_traffic() {
    let invalid = vec![
        Value::Null,
        json!({"name":"q","value":"private-query"}),
        json!("private-query"),
        json!([null]),
        json!([["q", "private-query"]]),
        json!([["q", "private-query", "extra"]]),
        json!([[]]),
        json!([[{"name":"q","value":"private-query"}]]),
        json!([{"name":"","value":"private-query"}]),
        json!([{"name":null,"value":"private-query"}]),
        json!([{"name":7,"value":"private-query"}]),
        json!([{"name":"q","value":null}]),
        json!([{"name":"q","value":7}]),
        json!([{"name":"q","value":false}]),
        json!([{"name":"q","value":["private-query"]}]),
        json!([{"name":"q","value":{"env":"RUSTA_QUERY_PRIVATE"}}]),
        json!([{"name":"q"}]),
        json!([{"value":"private-query"}]),
        json!([{"name":"q","value":"private-query","extra":"private-query"}]),
        json!([{"name":"q","value_env":"RUSTA_QUERY_PRIVATE"}]),
        json!([{"name":"é".to_owned()+&"n".repeat(127),"value":"private-query"}]),
        json!([{"name":"q","value":"é".to_owned()+&"v".repeat(4095)}]),
        Value::Array(vec![json!({"name":"q","value":"private-query"}); 33]),
        json!([{"name":"a","value":"x".repeat(4096)},{"name":"b","value":"x".repeat(4092)}]),
        json!([{"name":"q","value":"☃".repeat(1000)}]),
    ];
    for query in invalid {
        let temp = Temp::new();
        let server = Server::new();
        let p = plan_with_later_query(query);
        for execute in [false, true] {
            let output = command(&temp, &p, execute.then_some(&server))
                .env("RUSTA_QUERY_PRIVATE", "private-environment-value")
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "invalid query must fail before execution"
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("private-query"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private-query"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private-environment-value"));
            assert!(
                server.seen().is_empty(),
                "a valid setup/earlier request must wait for the entire plan"
            );
        }
    }
}

#[test]
fn query_pair_byte_count_and_serialized_size_limits_are_inclusive() {
    for (query, expected_query_bytes) in [
        (
            json!([{"name":"é".to_owned()+&"n".repeat(126),"value":""}]),
            133,
        ),
        (
            json!([{"name":"q","value":"é".to_owned()+&"v".repeat(4094)}]),
            4102,
        ),
        (Value::Array(vec![json!({"name":"a","value":""}); 32]), 95),
        (
            json!([{"name":"a","value":"x".repeat(4096)},{"name":"b","value":"x".repeat(4091)}]),
            8192,
        ),
    ] {
        let temp = Temp::new();
        let server = Server::new();
        let mut p = plan();
        p["requests"]["read"]["query"] = query;
        let r = report(command(&temp, &p, Some(&server)).output().unwrap(), true);
        reconcile(&r);
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].path.strip_prefix("/ok?").unwrap().len(),
            expected_query_bytes
        );
    }
}

#[test]
fn duplicate_query_keys_are_rejected_before_setup_or_measured_requests() {
    for raw_query in [
        r#"[{"name":"q","name":"other-private","value":"value-private"}]"#,
        r#"[{"name":"q","value":"value-private","value":"other-private"}]"#,
        r#"[],"query":[{"name":"q","value":"value-private"}]"#,
    ] {
        let temp = Temp::new();
        let server = Server::new();
        let p = plan_with_later_query(json!("QUERY_PLACEHOLDER"));
        let raw = serde_json::to_string(&p)
            .unwrap()
            .replace("\"QUERY_PLACEHOLDER\"", raw_query);
        let path = temp.0.join("plan.json");
        fs::write(&path, raw).unwrap();
        for execute in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"));
            cmd.current_dir(&temp.0)
                .args(["workload", "run"])
                .arg(&path);
            if execute {
                cmd.args([
                    "--execute",
                    "--base-url",
                    &server.url,
                    "--operation",
                    "read",
                    "--operation",
                    "z_later",
                ]);
            }
            let output = cmd.output().unwrap();
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("value-private"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("other-private"));
            assert!(server.seen().is_empty());
        }
    }
}

#[test]
fn query_preview_resolves_neither_credentials_nor_environment_interpolation() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["actors"]["a"] = json!({"headers_env":{"Cookie":"RUSTA_QUERY_MISSING_COOKIE"}});
    p["requests"]["read"]["query"] =
        json!([{"name":"preview-private+name","value":"${RUSTA_QUERY_VALUE}"}]);
    let output = command(&temp, &p, None)
        .args([
            "--base-url",
            &server.url,
            "--operation",
            "read",
            "--output",
            "preview.json",
        ])
        .env_remove("RUSTA_QUERY_MISSING_COOKIE")
        .env("RUSTA_QUERY_VALUE", "preview-environment-private")
        .output()
        .unwrap();
    let saved = fs::read_to_string(temp.0.join("preview.json")).unwrap();
    for text in [
        String::from_utf8_lossy(&output.stdout).as_ref(),
        String::from_utf8_lossy(&output.stderr).as_ref(),
        saved.as_str(),
    ] {
        for secret in [
            "preview-private+name",
            "preview-private%2Bname",
            "${RUSTA_QUERY_VALUE}",
            "%24%7BRUSTA_QUERY_VALUE%7D",
            "preview-environment-private",
        ] {
            assert!(!text.contains(secret));
        }
    }
    let r = report(output, true);
    assert_eq!(r["executed"], false);
    assert_eq!(r["runtime_secrets_loaded"], false);
    assert_eq!(r["schema_version"], 2);
    assert!(server.seen().is_empty());
}

#[test]
fn query_values_are_redacted_from_success_denial_transport_reports_and_evidence() {
    for (path, expected, success) in [
        ("/ok", None, true),
        ("/denied", Some(401), true),
        ("/denied", None, false),
        ("/disconnect", None, false),
    ] {
        let temp = Temp::new();
        let server = Server::new();
        let mut p = plan();
        p["requests"]["read"]["path"] = json!(path);
        p["requests"]["read"]["query"] = json!([{"name":"private+name","value":"private+query/☃"}]);
        if let Some(expected) = expected {
            p["requests"]["read"]["expect_status"] = json!(expected);
        }
        let output = command(&temp, &p, Some(&server))
            .args(["--output", "result.json"])
            .output()
            .unwrap();
        let saved = fs::read_to_string(temp.0.join("result.json")).unwrap();
        for text in [
            String::from_utf8_lossy(&output.stdout).as_ref(),
            String::from_utf8_lossy(&output.stderr).as_ref(),
            saved.as_str(),
        ] {
            for secret in [
                "private+name",
                "private%2Bname",
                "private+query/☃",
                "private%2Bquery%2F%E2%98%83",
            ] {
                assert!(
                    !text.contains(secret),
                    "query material must not enter diagnostics or evidence"
                );
            }
        }
        let r = report(output, success);
        reconcile(&r);
        assert_eq!(r["schema_version"], 2);
        assert_eq!(r["evidence"]["complete"], true);
        assert_eq!(r["evidence"]["records"].as_array().unwrap().len(), 1);
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].path,
            format!("{path}?private%2Bname=private%2Bquery%2F%E2%98%83")
        );
        assert_eq!(r["counts"]["passed"], u64::from(success));
        assert_eq!(r["counts"]["failed"], u64::from(!success));
    }
}

#[test]
fn query_support_keeps_explicit_selection_and_raw_path_query_restrictions() {
    let temp = Temp::new();
    let server = Server::new();
    let mut p = plan();
    p["requests"]["read"]["query"] = json!([{"name":"q","value":"selected-private"}]);
    let path = temp.plan(&p);
    let no_selection = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .current_dir(&temp.0)
        .args(["workload", "run"])
        .arg(&path)
        .args(["--execute", "--base-url", &server.url])
        .output()
        .unwrap();
    assert!(!no_selection.status.success());
    assert!(server.seen().is_empty());
    p["requests"]["read"]["path"] = json!("/ok?embedded=private");
    let invalid_path = command(&temp, &p, Some(&server)).output().unwrap();
    assert!(!invalid_path.status.success());
    assert!(server.seen().is_empty());
}

#[cfg(unix)]
#[test]
fn workload_fixtures_have_private_modes_and_protected_parents() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("workload-test-fixtures");
    let metadata = fs::symlink_metadata(&root).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.permissions().mode() & 0o022, 0);
    let target = fs::metadata(root.parent().unwrap()).unwrap();
    assert_eq!(target.permissions().mode() & 0o022, 0);
    let root = root.canonicalize().unwrap();
    assert_eq!(temp.0.parent(), Some(root.as_path()));
    assert_eq!(
        fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
