use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const MIB: usize = 1024 * 1024;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-pitest-input-limits-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        self.new_file(name, bytes).unwrap()
    }

    fn new_file(&self, name: &str, bytes: &[u8]) -> std::io::Result<String> {
        let path = self.path(name);
        let mut file = fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file.mode(0o600);
        }
        file.open(&path)?.write_all(bytes)?;
        Ok(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn temporary_fixtures_refuse_existing_files_and_have_private_permissions() {
    let directory = TempDir::new();
    let original = b"preserved synthetic fixture";
    let path = directory.file("existing.json", original);
    assert_eq!(
        directory
            .new_file("existing.json", b"replacement")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&directory.0).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        std::os::unix::fs::symlink(&path, directory.path("link.json")).unwrap();
        assert_eq!(
            directory
                .new_file("link.json", b"replacement")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

struct ContractServer {
    url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ContractServer {
    fn start(body: Vec<u8>, chunked: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/contract", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
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
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                assert!(line.starts_with("GET /contract HTTP/1.1"), "{line:?}");
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                worker_requests.fetch_add(1, Ordering::Relaxed);
                // An over-limit client may close the connection before consuming
                // the body, so broken-pipe errors are expected in rejection cases.
                if chunked {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    for chunk in body.chunks(37) {
                        if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                            || stream.write_all(chunk).is_err()
                            || stream.write_all(b"\r\n").is_err()
                        {
                            break;
                        }
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                } else {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ContractServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn run(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "input-limit command timed out: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn succeeded(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

fn rejected_without_output(output: &Output, destination: &str, diagnostic: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "{output:?}"
    );
    assert!(
        !Path::new(destination).exists(),
        "failed generation published a suite"
    );
}

fn contract() -> Value {
    json!({"openapi":"3.1.0","info":{"title":"Synthetic ☃ API","version":"1"},
        "paths":{"/health":{"get":{"operationId":"health","responses":{"200":{"description":"OK"}}}}}})
}

fn documents() -> [(&'static str, Vec<u8>); 2] {
    [
        ("json", serde_json::to_vec(&contract()).unwrap()),
        ("yaml", "openapi: 3.1.0\ninfo: {title: 'Synthetic ☃ API', version: '1'}\npaths:\n  /health:\n    get:\n      operationId: health\n      responses:\n        '200': {description: OK}\n".as_bytes().to_vec()),
    ]
}

#[test]
fn contract_input_boundary_is_inclusive_for_json_yaml_files_and_http_bodies() {
    let directory = TempDir::new();
    for (format, bytes) in documents() {
        let source = directory.file(&format!("contract.{format}"), &bytes);
        let exact = bytes.len().to_string();
        let below = (bytes.len() - 1).to_string();
        let destination = directory.path(&format!("file-{format}.suite.json"));
        succeeded(&run(&[
            "openapi",
            "generate",
            &source,
            "--out",
            &destination,
            "--max-input-bytes",
            &exact,
        ]));
        let rejected = directory.path(&format!("file-{format}-rejected.json"));
        rejected_without_output(
            &run(&[
                "openapi",
                "generate",
                &source,
                "--out",
                &rejected,
                "--max-input-bytes",
                &below,
            ]),
            &rejected,
            "input limit",
        );
        for chunked in [false, true] {
            let server = ContractServer::start(bytes.clone(), chunked);
            let destination = directory.path(&format!("http-{format}-{chunked}.suite.json"));
            succeeded(&run(&[
                "openapi",
                "generate",
                &server.url,
                "--out",
                &destination,
                "--max-input-bytes",
                &exact,
            ]));
            let rejected = directory.path(&format!("http-{format}-{chunked}-rejected.json"));
            rejected_without_output(
                &run(&[
                    "openapi",
                    "generate",
                    &server.url,
                    "--out",
                    &rejected,
                    "--max-input-bytes",
                    &below,
                ]),
                &rejected,
                "input limit",
            );
            assert_eq!(server.requests.load(Ordering::Relaxed), 2);
        }
    }
}

#[test]
fn raised_contract_input_bound_accepts_large_valid_json_and_yaml() {
    let directory = TempDir::new();
    for (format, mut bytes) in documents() {
        bytes.resize(MIB + 1, b' ');
        let exact = bytes.len().to_string();
        let source = directory.file(&format!("large.{format}"), &bytes);
        let rejected = directory.path(&format!("large-{format}-default.json"));
        rejected_without_output(
            &run(&["openapi", "generate", &source, "--out", &rejected]),
            &rejected,
            "input limit",
        );
        let destination = directory.path(&format!("large-{format}.suite.json"));
        succeeded(&run(&[
            "openapi",
            "generate",
            &source,
            "--out",
            &destination,
            "--max-input-bytes",
            &exact,
        ]));
        succeeded(&run(&["suite", "run", &destination]));
        let server = ContractServer::start(bytes, false);
        let rejected = directory.path(&format!("large-http-{format}-default.json"));
        rejected_without_output(
            &run(&["openapi", "generate", &server.url, "--out", &rejected]),
            &rejected,
            "input limit",
        );
        let destination = directory.path(&format!("large-http-{format}.suite.json"));
        succeeded(&run(&[
            "openapi",
            "generate",
            &server.url,
            "--out",
            &destination,
            "--max-input-bytes",
            &exact,
        ]));
        assert_eq!(server.requests.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn generated_suite_and_loader_byte_boundaries_include_the_final_newline() {
    let directory = TempDir::new();
    let source = directory.file("contract.json", &serde_json::to_vec(&contract()).unwrap());
    let original = directory.path("original.suite.json");
    succeeded(&run(&["openapi", "generate", &source, "--out", &original]));
    let bytes = fs::read(&original).unwrap();
    assert!(bytes.ends_with(b"\n"));
    let exact = bytes.len().to_string();
    let below = (bytes.len() - 1).to_string();
    let destination = directory.path("exact.suite.json");
    succeeded(&run(&[
        "openapi",
        "generate",
        &source,
        "--out",
        &destination,
        "--max-suite-bytes",
        &exact,
    ]));
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    let rejected = directory.path("below.suite.json");
    rejected_without_output(
        &run(&[
            "openapi",
            "generate",
            &source,
            "--out",
            &rejected,
            "--max-suite-bytes",
            &below,
        ]),
        &rejected,
        "Generated suite exceeds",
    );
    succeeded(&run(&[
        "suite",
        "run",
        &original,
        "--max-input-bytes",
        &exact,
    ]));
    let output = run(&["suite", "run", &original, "--max-input-bytes", &below]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("suite exceeds the configured input limit"),
        "{output:?}"
    );
}

#[test]
fn larger_generated_suites_require_coherent_output_and_loading_limits() {
    let directory = TempDir::new();
    let mut large = contract();
    large["x-padding"] = Value::String("x".repeat(4 * MIB));
    let source = directory.file("large-contract.json", &serde_json::to_vec(&large).unwrap());
    let raised = (8 * MIB).to_string();
    let rejected = directory.path("default-output.suite.json");
    rejected_without_output(
        &run(&[
            "openapi",
            "generate",
            &source,
            "--out",
            &rejected,
            "--max-input-bytes",
            &raised,
        ]),
        &rejected,
        "Generated suite exceeds",
    );
    let destination = directory.path("raised-output.suite.json");
    succeeded(&run(&[
        "openapi",
        "generate",
        &source,
        "--out",
        &destination,
        "--max-input-bytes",
        &raised,
        "--max-suite-bytes",
        &raised,
    ]));
    let size = fs::metadata(&destination).unwrap().len();
    assert!(size > (4 * MIB) as u64 && size < (8 * MIB) as u64);
    let output = run(&["suite", "run", &destination]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("suite exceeds the configured input limit"),
        "{output:?}"
    );
    succeeded(&run(&[
        "suite",
        "run",
        &destination,
        "--max-input-bytes",
        &size.to_string(),
    ]));
}

#[test]
fn configured_byte_limits_reject_invalid_ranges_and_enforce_small_valid_bounds() {
    let directory = TempDir::new();
    let source = directory.file("contract.json", &serde_json::to_vec(&contract()).unwrap());
    let suite = directory.path("valid.suite.json");
    let upper = (64 * MIB).to_string();
    succeeded(&run(&[
        "openapi",
        "generate",
        &source,
        "--out",
        &suite,
        "--max-input-bytes",
        &upper,
        "--max-suite-bytes",
        &upper,
    ]));
    succeeded(&run(&["suite", "run", &suite, "--max-input-bytes", &upper]));
    for invalid in ["0".to_owned(), (64 * MIB + 1).to_string()] {
        for flag in ["--max-input-bytes", "--max-suite-bytes"] {
            let destination = directory.path(&format!("invalid-{flag}-{invalid}.json"));
            let output = run(&[
                "openapi",
                "generate",
                &source,
                "--out",
                &destination,
                flag,
                &invalid,
            ]);
            assert_eq!(output.status.code(), Some(2), "{output:?}");
            assert!(!Path::new(&destination).exists());
        }
        let output = run(&["suite", "run", &suite, "--max-input-bytes", &invalid]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
    }
    for (flag, diagnostic) in [
        ("--max-input-bytes", "input limit"),
        ("--max-suite-bytes", "Generated suite exceeds"),
    ] {
        let destination = directory.path(&format!("too-small-{flag}.json"));
        rejected_without_output(
            &run(&[
                "openapi",
                "generate",
                &source,
                "--out",
                &destination,
                flag,
                "1",
            ]),
            &destination,
            diagnostic,
        );
    }
    let output = run(&["suite", "run", &suite, "--max-input-bytes", "1"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("suite exceeds the configured input limit"),
        "{output:?}"
    );
}

#[test]
fn raised_byte_limits_preserve_json_yaml_and_suite_complexity_guards() {
    let directory = TempDir::new();
    let raised = (8 * MIB).to_string();
    for (name, payload) in [
        ("depth", format!("{}0{}", "[".repeat(70), "]".repeat(70))),
        ("nodes", format!("[{}0]", "0,".repeat(100_000))),
    ] {
        for (format, mut document) in documents() {
            if format == "json" {
                document.pop();
                document.extend_from_slice(format!(",\"x-{name}\":{payload}}}").as_bytes());
            } else {
                document.extend_from_slice(format!("x-{name}: {payload}\n").as_bytes());
            }
            let source = directory.file(&format!("{name}.{format}"), &document);
            let destination = directory.path(&format!("{name}-{format}.suite.json"));
            rejected_without_output(
                &run(&[
                    "openapi",
                    "generate",
                    &source,
                    "--out",
                    &destination,
                    "--max-input-bytes",
                    &raised,
                    "--max-suite-bytes",
                    &raised,
                ]),
                &destination,
                "nesting or node limit",
            );
        }
        // Suite loading uses the same strict parser before structure validation.
        let suite = directory.file(
            &format!("{name}.suite.json"),
            format!("{{\"x-{name}\":{payload}}}").as_bytes(),
        );
        let output = run(&["suite", "run", &suite, "--max-input-bytes", &raised]);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("nesting or node limit"),
            "{output:?}"
        );
    }
}
