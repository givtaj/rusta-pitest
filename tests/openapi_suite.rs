use std::{
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

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-pitest-openapi-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> String {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }

    fn read_json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.0.join(name)).unwrap()).unwrap()
    }

    fn generate(&self, contract: &Value) -> String {
        let source = self.write("contract.json", serde_json::to_vec(contract).unwrap());
        let suite = self.path("suite.json");
        assert_exit(
            &run(&self.0, &["openapi", "generate", &source, "--out", &suite]),
            true,
        );
        suite
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
    headers: Vec<(String, String)>,
}

struct Response {
    status: u16,
    content_type: Option<&'static str>,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn write(self, stream: &mut TcpStream) {
        write!(
            stream,
            "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.body.len()
        )
        .unwrap();
        if let Some(content_type) = self.content_type {
            write!(stream, "Content-Type: {content_type}\r\n").unwrap();
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
        Self::start_socket(move |request, stream| handler(request).write(stream))
    }

    fn start_socket(handler: impl Fn(&Request, &mut TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
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
                // macOS can inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    continue;
                }
                let mut words = line.split_whitespace();
                let method = words.next().unwrap().to_owned();
                let path = words.next().unwrap().to_owned();
                let mut headers = Vec::new();
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
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
                let request = Request {
                    method,
                    path,
                    headers,
                };
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

fn run(directory: &Path, args: &[&str]) -> Output {
    run_env(directory, args, &[])
}

fn run_env(directory: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"));
    command
        .current_dir(directory)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    command.env_remove("RUSTA_SUITE_MISSING_AUTH_TOKEN");
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "suite command exceeded ten seconds: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn assert_exit(output: &Output, success: bool) {
    assert_eq!(output.status.success(), success, "{output:?}");
}

fn contract(paths: Value) -> Value {
    json!({"openapi":"3.1.0","info":{"title":"Synthetic API","version":"1"},"paths":paths})
}

fn response(schema: Value) -> Value {
    json!({"200":{"description":"OK","content":{"application/json":{"schema":schema}}}})
}

fn health_contract() -> Value {
    contract(
        json!({"/health":{"get":{"operationId":"health","responses":response(json!({
            "type":"object","required":["status"],"properties":{"status":{"const":"ok"}}
        }))}}}),
    )
}

fn widget_contract(authenticated: bool) -> Value {
    let mut result = contract(json!({"/widgets/{id}":{"get":{
        "operationId":"getWidget",
        "parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string"}}],
        "responses":response(json!({"$ref":"#/components/schemas/Widget"}))
    }}}));
    result["components"] = json!({"schemas":{"Widget":{
        "type":"object","required":["id","name"],
        "properties":{"id":{"type":"string"},"name":{"type":"string"}}
    }}});
    if authenticated {
        result["security"] = json!([{"bearer":[]}]);
        result["components"]["securitySchemes"] =
            json!({"bearer":{"type":"http","scheme":"bearer"}});
    }
    result
}

#[test]
fn json_and_yaml_generation_is_deterministic_and_preserves_existing_outputs() {
    let directory = TempDir::new();
    let suite = directory.generate(&health_contract());
    let original = fs::read(&suite).unwrap();
    let second = directory.path("second.json");
    let source = directory.path("contract.json");
    assert_exit(
        &run(
            &directory.0,
            &["openapi", "generate", &source, "--out", &second],
        ),
        true,
    );
    assert_eq!(original, fs::read(&second).unwrap());
    assert_exit(
        &run(
            &directory.0,
            &["openapi", "generate", &source, "--out", &suite],
        ),
        false,
    );
    assert_eq!(original, fs::read(&suite).unwrap());
    let yaml = directory.write(
        "contract.yaml",
        br#"openapi: 3.1.0
info: {title: Synthetic API, version: "1"}
paths:
  /health:
    get:
      operationId: health
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [status]
                properties:
                  status: {const: ok}
"#,
    );
    let yaml_suite = directory.path("yaml-suite.json");
    assert_exit(
        &run(
            &directory.0,
            &["openapi", "generate", &yaml, "--out", &yaml_suite],
        ),
        true,
    );
    assert_eq!(
        directory.read_json("suite.json")["cases"],
        directory.read_json("yaml-suite.json")["cases"]
    );
}

#[test]
fn invalid_contracts_fail_without_creating_a_suite() {
    let directory = TempDir::new();
    for (index, bytes) in [
        r#"{"openapi":"3.1.0","info":{"title":"Test","version":"1"},"paths":{"/health":{"get":{"responses":{"200":{"description":"OK"}},"responses":{}}}}}"#,
        "openapi: 3.1.0\ninfo: {title: Test, version: '1'}\npaths: {}\npaths: {}\n",
        "<!DOCTYPE html><html><title>Swagger UI</title></html>",
        r#"{"openapi":"3.0.3","info":{"title":"Test","version":"1"},"paths":{}}"#,
    ].into_iter().enumerate() {
        let source = directory.write(&format!("invalid-{index}.txt"), bytes);
        let suite = directory.path(&format!("invalid-{index}.suite.json"));
        let output = run(&directory.0, &["openapi", "generate", &source, "--out", &suite]);
        assert_exit(&output, false);
        assert!(!Path::new(&suite).exists());
        if index == 2 {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            assert!(stderr.contains("html") && (stderr.contains("raw") || stderr.contains("openapi")), "{output:?}");
        }
    }
}

#[test]
fn generation_from_url_only_fetches_the_contract() {
    let directory = TempDir::new();
    let spec = health_contract();
    let server = Server::start(move |_| Response::json(200, spec.clone()));
    let source = format!("{}/openapi.json", server.url);
    let suite = directory.path("suite.json");
    assert_exit(
        &run(
            &directory.0,
            &["openapi", "generate", &source, "--out", &suite],
        ),
        true,
    );
    let requests = server.finish();
    assert_eq!(
        requests.len(),
        1,
        "generation must not exercise operation endpoints"
    );
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/openapi.json");
    assert_eq!(
        directory.read_json("suite.json")["cases"][0]["id"],
        "health"
    );
}

#[test]
fn preview_and_existing_report_refusal_send_no_operation_requests() {
    let directory = TempDir::new();
    let suite = directory.generate(&health_contract());
    let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
    let report = directory.path("preview.json");
    let preview = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--output",
            &report,
        ],
    );
    assert_exit(&preview, true);
    assert_eq!(directory.read_json("preview.json")["kind"], "suite");
    let original = fs::read(&report).unwrap();
    let refusal = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--execute",
            "--output",
            &report,
        ],
    );
    assert_exit(&refusal, false);
    assert_eq!(fs::read(&report).unwrap(), original);
    assert!(server.finish().is_empty());
}

#[test]
fn authenticated_execution_preserves_base_prefix_escapes_fixture_and_redacts_reports() {
    let directory = TempDir::new();
    let suite = directory.generate(&widget_contract(true));
    let server = Server::start(|_| {
        Response::json(
            200,
            json!({
                "id":"response-private-id","name":"response-private-name"
            }),
        )
    });
    let base = format!("{}/api/v1", server.url);
    let report = directory.path("result.json");
    let output = run_env(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &base,
            "--operation",
            "getWidget",
            "--fixture",
            "getWidget.path.id=synthetic/id",
            "--auth-env",
            "bearer=RUSTA_SUITE_TEST_TOKEN",
            "--execute",
            "--output",
            &report,
        ],
        &[("RUSTA_SUITE_TEST_TOKEN", "suite-token-secret")],
    );
    assert_exit(&output, true);
    assert_eq!(directory.read_json("result.json")["passed"], true);
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/v1/widgets/synthetic%2Fid");
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer suite-token-secret")
    );
    let recorded = format!(
        "{}{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(&report).unwrap()
    );
    for secret in [
        "suite-token-secret",
        "response-private-id",
        "response-private-name",
        "synthetic/id",
    ] {
        assert!(
            !recorded.contains(secret),
            "sensitive request/response value persisted: {secret}"
        );
    }
}

#[test]
fn simple_path_fixtures_reach_the_wire_encoded_once_with_literal_and_base_prefixes() {
    let directory = TempDir::new();
    let mut contract = widget_contract(false);
    let operation = contract["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/widgets/{id}")
        .unwrap();
    contract["paths"]["/literal;part,part/prefix-{id}.json"] = operation;
    let suite = directory.generate(&contract);
    let server = Server::start(|_| Response::json(200, json!({"id":"w1","name":"Widget"})));
    let base = format!("{}/api/encoded%2Fprefix/", server.url);
    let fixtures = [
        ("a;b,c", "a%3Bb%2Cc"),
        ("a/b?x#y", "a%2Fb%3Fx%23y"),
        ("%2F%2e%2e", "%252F%252e%252e"),
        ("☃/{id}", "%E2%98%83%2F%7Bid%7D"),
    ];
    for (fixture, _) in fixtures {
        let binding = format!("getWidget.path.id={fixture}");
        assert_exit(
            &run(
                &directory.0,
                &[
                    "suite",
                    "run",
                    &suite,
                    "--base-url",
                    &base,
                    "--operation",
                    "getWidget",
                    "--fixture",
                    &binding,
                    "--execute",
                ],
            ),
            true,
        );
    }
    let requests = server.finish();
    assert_eq!(requests.len(), fixtures.len());
    for (request, (_, encoded)) in requests.iter().zip(fixtures) {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            format!("/api/encoded%2Fprefix/literal;part,part/prefix-{encoded}.json")
        );
    }
}

#[test]
fn invalid_response_schema_and_documented_error_status_fail_positive_cases() {
    for (status, body) in [
        (200, json!({"id":"fixture-without-name"})),
        (401, json!({"error":"documented error"})),
    ] {
        let directory = TempDir::new();
        let mut spec = widget_contract(false);
        spec["paths"]["/widgets/{id}"]["get"]["responses"]["401"] = json!({"description":"Not authorized","content":{"application/json":{"schema":{"type":"object"}}}});
        let suite = directory.generate(&spec);
        let server = Server::start(move |_| Response::json(status, body.clone()));
        let report = directory.path("result.json");
        let output = run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "getWidget",
                "--fixture",
                "getWidget.path.id=w_001",
                "--execute",
                "--output",
                &report,
            ],
        );
        assert_exit(&output, false);
        assert_eq!(directory.read_json("result.json")["passed"], false);
        let recorded = format!(
            "{}{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(&report).unwrap()
        );
        assert!(!recorded.contains("fixture-without-name"));
        assert!(!recorded.contains("documented error"));
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn whole_selection_preflight_prevents_requests_when_a_fixture_or_auth_is_missing() {
    for (authenticated, fixtures) in [(false, false), (true, true)] {
        let directory = TempDir::new();
        let mut spec = widget_contract(authenticated);
        spec["paths"]["/health"] = health_contract()["paths"]["/health"].clone();
        // The first selected case is runnable; the second must stop the whole run.
        spec["paths"]["/health"]["get"]["security"] = json!([]);
        let suite = directory.generate(&spec);
        let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
        let mut args = vec![
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--operation",
            "getWidget",
            "--execute",
        ];
        if fixtures {
            args.extend(["--fixture", "getWidget.path.id=w_001"]);
        }
        assert_exit(&run(&directory.0, &args), false);
        assert!(server.finish().is_empty());
    }
}

#[test]
fn writes_remain_visible_and_cannot_be_executed() {
    let directory = TempDir::new();
    let spec = contract(json!({"/widgets":{"post":{
        "operationId":"createWidget","requestBody":{"required":true,"content":{"application/json":{
            "schema":{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}
        }}},"responses":{"201":{"description":"Created"}}
    }}}));
    let suite = directory.generate(&spec);
    let mut generated = directory.read_json("suite.json");
    assert_eq!(generated["cases"][0]["method"], "POST");
    assert!(
        !generated["cases"][0]["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Editable suite metadata cannot remove the runner's method restriction.
    generated["cases"][0]["blockers"] = json!([]);
    fs::write(&suite, serde_json::to_vec(&generated).unwrap()).unwrap();
    let server = Server::start(|_| Response::json(201, json!({})));
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "createWidget",
            "--execute",
        ],
    );
    assert_exit(&output, false);
    assert!(server.finish().is_empty());
}

#[test]
fn external_references_are_blocked_without_fetching_them() {
    let directory = TempDir::new();
    let server = Server::start(|_| Response::json(200, json!({"type":"object"})));
    let external = format!("{}/unapproved-schema.json", server.url);
    let spec = contract(json!({"/health":{"get":{
        "operationId":"health","responses":response(json!({"$ref":external}))
    }}}));
    let suite = directory.generate(&spec);
    let generated = directory.read_json("suite.json");
    assert!(
        !generated["cases"][0]["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_exit(
        &run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--execute",
            ],
        ),
        false,
    );
    assert!(server.finish().is_empty());
}

#[test]
fn expanded_parameter_and_response_schemas_are_blocked_before_traffic() {
    for parameter in [false, true] {
        let directory = TempDir::new();
        let server = Server::start(|_| Response::json(200, json!("synthetic")));
        let mut schemas = serde_json::Map::new();
        schemas.insert("n0".into(), json!({"type":"string","minLength":1}));
        for level in 1..=12 {
            let reference = format!("#/components/schemas/n{}", level - 1);
            schemas.insert(
                format!("n{level}"),
                json!({"allOf":[
                    {"$ref":reference}, {"$ref":reference}
                ]}),
            );
        }
        let schema = json!({"type":"string","allOf":[{"$ref":"#/components/schemas/n12"}]});
        let mut spec = contract(json!({"/health":{"get":{
            "operationId":"health","responses":response(json!(true))
        }}}));
        spec["components"] = json!({"schemas":schemas});
        if parameter {
            spec["paths"]["/health"]["get"]["parameters"] = json!([
                {"name":"value","in":"query","required":true,"schema":schema}
            ]);
        } else {
            spec["paths"]["/health"]["get"]["responses"] = response(schema);
        }
        let suite = directory.generate(&spec);
        assert!(
            directory.read_json("suite.json")["cases"][0]["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("schema"))
        );
        assert_exit(
            &run(
                &directory.0,
                &[
                    "suite",
                    "run",
                    &suite,
                    "--base-url",
                    &server.url,
                    "--operation",
                    "health",
                    "--execute",
                ],
            ),
            false,
        );
        assert!(server.finish().is_empty());
    }
}

#[test]
fn head_and_bodyless_successes_do_not_require_json_bodies() {
    let directory = TempDir::new();
    let spec = contract(json!({
        "/health":{"head":{"operationId":"headHealth","responses":response(json!({"type":"object","required":["status"]}))}},
        "/empty":{"get":{"operationId":"empty","responses":{"204":{"description":"No content"}}}}
    }));
    let suite = directory.generate(&spec);
    let server = Server::start(|request| Response {
        status: if request.method == "HEAD" { 200 } else { 204 },
        content_type: None,
        body: Vec::new(),
    });
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "headHealth",
            "--operation",
            "empty",
            "--execute",
        ],
    );
    assert_exit(&output, true);
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().any(|request| request.method == "HEAD"));
}

#[test]
fn required_query_arrays_preserve_repeated_keys() {
    let directory = TempDir::new();
    let mut spec = health_contract();
    spec["paths"]["/health"]["get"]["parameters"] = json!([{
        "name":"tags","in":"query","required":true,"style":"form","explode":true,
        "schema":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"string","enum":["alpha","beta"]}}
    }]);
    let suite = directory.generate(&spec);
    let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
    assert_exit(
        &run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--execute",
            ],
        ),
        true,
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    let query = requests[0].path.split_once('?').unwrap().1;
    assert_eq!(
        query
            .split('&')
            .filter(|pair| pair.starts_with("tags="))
            .count(),
        2
    );
}

#[test]
fn malformed_media_and_unsupported_response_headers_are_blocked_before_traffic() {
    for invalid_response in [
        json!({"200":{"description":"OK","content":{"application/json":null}}}),
        json!({"200":{"description":"OK","headers":{"X-Limit":{
            "required":"true","schema":{"type":"integer"}
        }}}}),
        json!({"200":{"description":"OK","headers":{"X-Values":{
            "required":true,"schema":{"type":"array","items":{"type":"integer"}}
        }}}}),
    ] {
        let directory = TempDir::new();
        let mut spec = health_contract();
        spec["paths"]["/health"]["get"]["responses"] = invalid_response;
        let suite = directory.generate(&spec);
        let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
        let output = run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--execute",
            ],
        );
        assert_exit(&output, false);
        assert!(server.finish().is_empty());
    }
}

#[test]
fn default_response_cannot_supply_a_positive_success_oracle() {
    let directory = TempDir::new();
    let mut spec = health_contract();
    spec["paths"]["/health"]["get"]["responses"]["default"] = json!({
        "description":"Unexpected response",
        "content":{"application/json":{"schema":{"type":"object"}}}
    });
    let suite = directory.generate(&spec);
    let server =
        Server::start(|_| Response::json(299, json!({"error":"unexpected-success-status"})));
    let report = directory.path("result.json");
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--execute",
            "--output",
            &report,
        ],
    );
    assert_exit(&output, false);
    assert_eq!(directory.read_json("result.json")["passed"], false);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn json_media_without_a_schema_still_requires_well_formed_json() {
    let directory = TempDir::new();
    let spec = contract(json!({"/health":{"get":{
        "operationId":"health","responses":{"200":{
            "description":"OK","content":{"application/json":{}}
        }}
    }}}));
    let suite = directory.generate(&spec);
    let server = Server::start(|_| Response {
        status: 200,
        content_type: Some("application/json"),
        body: b"this is not JSON".to_vec(),
    });
    let report = directory.path("result.json");
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--execute",
            "--output",
            &report,
        ],
    );
    assert_exit(&output, false);
    assert_eq!(directory.read_json("result.json")["passed"], false);
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn json_suffix_wildcard_matches_concrete_json_media_types() {
    let directory = TempDir::new();
    let spec = contract(json!({"/health":{"get":{
        "operationId":"health","responses":{"200":{
            "description":"OK","content":{"application/*+json":{"schema":{
                "type":"object","required":["status"],"properties":{"status":{"const":"ok"}}
            }}}
        }}
    }}}));
    let suite = directory.generate(&spec);
    let server = Server::start(|_| Response {
        status: 200,
        content_type: Some("application/problem+json"),
        body: br#"{"status":"ok"}"#.to_vec(),
    });
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--execute",
        ],
    );
    assert_exit(&output, true);
    assert_eq!(server.finish().len(), 1);
}

fn authentication_contract(requirements: Value) -> Value {
    let mut spec = health_contract();
    spec["security"] = requirements;
    spec["components"] = json!({"securitySchemes":{
        "bearer":{"type":"http","scheme":"bearer"},
        "apiKey":{"type":"apiKey","in":"header","name":"X-Api-Key"},
        "secondKey":{"type":"apiKey","in":"header","name":"X-Api-Key"}
    }});
    spec
}

fn has_header(request: &Request, name: &str, value: &str) -> bool {
    request.headers.iter().any(|(actual_name, actual_value)| {
        actual_name.eq_ignore_ascii_case(name) && actual_value == value
    })
}

fn assert_redacted(output: &Output, report: &Path, secrets: &[&str]) {
    let recorded = format!(
        "{}{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(report).unwrap()
    );
    for secret in secrets {
        assert!(
            !recorded.contains(secret),
            "sensitive data persisted: {secret}"
        );
    }
}

#[test]
fn header_api_key_authentication_uses_the_environment_without_persisting_it() {
    let directory = TempDir::new();
    let suite = directory.generate(&authentication_contract(json!([{"apiKey":[]}])));
    let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
    let report = directory.path("result.json");
    let output = run_env(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--auth-env",
            "apiKey=RUSTA_SUITE_API_KEY",
            "--execute",
            "--output",
            &report,
        ],
        &[("RUSTA_SUITE_API_KEY", "api-key-private-value")],
    );
    assert_exit(&output, true);
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(has_header(
        &requests[0],
        "x-api-key",
        "api-key-private-value"
    ));
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name == "authorization")
    );
    assert_redacted(&output, Path::new(&report), &["api-key-private-value"]);
}

#[test]
fn alternative_authentication_falls_back_after_missing_or_invalid_first_credentials() {
    for (first_variable, environment) in [
        (
            "RUSTA_SUITE_MISSING_AUTH_TOKEN",
            vec![("RUSTA_SUITE_API_KEY", "fallback-private-key")],
        ),
        (
            "RUSTA_SUITE_INVALID_TOKEN",
            vec![
                ("RUSTA_SUITE_INVALID_TOKEN", "invalid\nheader-secret"),
                ("RUSTA_SUITE_API_KEY", "fallback-private-key"),
            ],
        ),
    ] {
        let directory = TempDir::new();
        let suite = directory.generate(&authentication_contract(json!([
            {"bearer":[]}, {"apiKey":[]}
        ])));
        let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
        let first_binding = format!("bearer={first_variable}");
        let report = directory.path("result.json");
        let output = run_env(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--auth-env",
                &first_binding,
                "--auth-env",
                "apiKey=RUSTA_SUITE_API_KEY",
                "--execute",
                "--output",
                &report,
            ],
            &environment,
        );
        assert_exit(&output, true);
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert!(has_header(
            &requests[0],
            "x-api-key",
            "fallback-private-key"
        ));
        assert!(
            !requests[0]
                .headers
                .iter()
                .any(|(name, _)| name == "authorization")
        );
        assert_redacted(
            &output,
            Path::new(&report),
            &["fallback-private-key", "header-secret"],
        );
    }
}

#[test]
fn combined_authentication_requires_every_binding_and_credential_before_traffic() {
    for configuration in ["missing_binding", "missing_environment", "complete"] {
        let directory = TempDir::new();
        let suite = directory.generate(&authentication_contract(json!([
            {"bearer":[], "apiKey":[]}
        ])));
        let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
        let report = directory.path("result.json");
        let mut args = vec![
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--auth-env",
            "bearer=RUSTA_SUITE_AND_TOKEN",
            "--execute",
            "--output",
            &report,
        ];
        if configuration != "missing_binding" {
            args.extend([
                "--auth-env",
                if configuration == "missing_environment" {
                    "apiKey=RUSTA_SUITE_MISSING_AUTH_TOKEN"
                } else {
                    "apiKey=RUSTA_SUITE_AND_KEY"
                },
            ]);
        }
        let output = run_env(
            &directory.0,
            &args,
            &[
                ("RUSTA_SUITE_AND_TOKEN", "and-private-token"),
                ("RUSTA_SUITE_AND_KEY", "and-private-key"),
            ],
        );
        assert_exit(&output, configuration == "complete");
        let requests = server.finish();
        if configuration == "complete" {
            assert_eq!(requests.len(), 1);
            assert!(has_header(
                &requests[0],
                "authorization",
                "Bearer and-private-token"
            ));
            assert!(has_header(&requests[0], "x-api-key", "and-private-key"));
            assert_redacted(
                &output,
                Path::new(&report),
                &["and-private-token", "and-private-key"],
            );
        } else {
            assert!(
                requests.is_empty(),
                "a partial AND requirement must not send a request"
            );
            assert!(!Path::new(&report).exists());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("and-private"));
        }
    }
}

#[test]
fn conflicting_security_or_parameter_headers_are_refused_before_traffic() {
    for parameter_conflict in [false, true] {
        let directory = TempDir::new();
        let mut spec = authentication_contract(if parameter_conflict {
            json!([{"apiKey":[]}])
        } else {
            json!([{"apiKey":[], "secondKey":[]}])
        });
        if parameter_conflict {
            spec["paths"]["/health"]["get"]["parameters"] = json!([{
                "name":"X-Api-Key", "in":"header", "required":true,
                "schema":{"type":"string"}
            }]);
        }
        let suite = directory.generate(&spec);
        let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
        let output = run_env(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--auth-env",
                "apiKey=RUSTA_SUITE_COLLISION_KEY",
                "--auth-env",
                "secondKey=RUSTA_SUITE_COLLISION_KEY",
                "--execute",
            ],
            &[("RUSTA_SUITE_COLLISION_KEY", "collision-private-key")],
        );
        assert_exit(&output, false);
        assert!(server.finish().is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("collision-private-key"));
    }
}

fn two_operation_contract() -> Value {
    let mut spec = health_contract();
    spec["paths"]["/other"] = spec["paths"]["/health"].clone();
    spec["paths"]["/other"]["get"]["operationId"] = json!("other");
    spec
}

#[test]
fn selected_request_budget_is_enforced_before_any_request() {
    let directory = TempDir::new();
    let suite = directory.generate(&two_operation_contract());
    let server = Server::start(|_| Response::json(200, json!({"status":"ok"})));
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--operation",
            "other",
            "--max-requests",
            "1",
            "--execute",
        ],
    );
    assert_exit(&output, false);
    assert!(String::from_utf8_lossy(&output.stderr).contains("max-requests"));
    assert!(server.finish().is_empty());
}

#[test]
fn sequential_suite_requests_respect_the_configured_start_rate() {
    for rate in [1, 4] {
        let directory = TempDir::new();
        let suite = directory.generate(&two_operation_contract());
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let server_arrivals = Arc::clone(&arrivals);
        let server = Server::start(move |_| {
            server_arrivals.lock().unwrap().push(Instant::now());
            Response::json(200, json!({"status":"ok"}))
        });
        let rate_text = rate.to_string();
        let output = run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "health",
                "--operation",
                "other",
                "--rate-per-second",
                &rate_text,
                "--execute",
            ],
        );
        assert_exit(&output, true);
        assert_eq!(server.finish().len(), 2);
        let arrivals = arrivals.lock().unwrap();
        assert_eq!(arrivals.len(), 2);
        // Scheduling/network receive jitter can shorten the observed gap slightly.
        // A missing limiter would send these two immediate responses together.
        let minimum = Duration::from_secs_f64(0.8 / f64::from(rate));
        assert!(arrivals[1].duration_since(arrivals[0]) >= minimum);
    }
}

#[test]
fn timeout_in_the_response_body_is_a_failed_case_with_sanitized_diagnostics() {
    let directory = TempDir::new();
    let suite = directory.generate(&health_contract());
    let server = Server::start_socket(|_, stream| {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\nprivate-timeout-prefix").unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(300));
        let _ = stream.write_all(b"private-timeout-tail");
    });
    let report = directory.path("result.json");
    let output = run(
        &directory.0,
        &[
            "suite",
            "run",
            &suite,
            "--base-url",
            &server.url,
            "--operation",
            "health",
            "--timeout-ms",
            "100",
            "--execute",
            "--output",
            &report,
        ],
    );
    assert_exit(&output, false);
    assert_eq!(server.finish().len(), 1);
    let result = directory.read_json("result.json");
    assert_eq!(result["passed"], false);
    assert_eq!(result["cases"][0]["status"], 200);
    assert_eq!(result["cases"][0]["reasons"], json!(["timeout"]));
    assert_redacted(
        &output,
        Path::new(&report),
        &["private-timeout-prefix", "private-timeout-tail"],
    );
}

#[test]
fn response_size_limit_is_inclusive_and_oversized_payloads_are_not_persisted() {
    const LIMIT: usize = 1024 * 1024;
    for (size, success) in [(LIMIT, true), (LIMIT + 1, false)] {
        let directory = TempDir::new();
        let spec = contract(json!({"/payload":{"get":{
            "operationId":"payload", "responses":response(json!({"type":"string"}))
        }}}));
        let suite = directory.generate(&spec);
        let mut body = vec![b'x'; size];
        body[0] = b'"';
        body[size - 1] = b'"';
        let secret = b"oversized-private-response";
        body[1..1 + secret.len()].copy_from_slice(secret);
        let server = Server::start_socket(move |_, stream| {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            // The client may stop reading immediately once its size budget is exceeded.
            let _ = stream.write_all(&body);
        });
        let report = directory.path("result.json");
        let output = run(
            &directory.0,
            &[
                "suite",
                "run",
                &suite,
                "--base-url",
                &server.url,
                "--operation",
                "payload",
                "--execute",
                "--output",
                &report,
            ],
        );
        assert_exit(&output, success);
        assert_eq!(server.finish().len(), 1);
        let result = directory.read_json("result.json");
        assert_eq!(result["passed"], success);
        if !success {
            assert_eq!(
                result["cases"][0]["reasons"],
                json!(["response_size_limit_exceeded"])
            );
        }
        assert_redacted(&output, Path::new(&report), &["oversized-private-response"]);
    }
}
