use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};

const COOKIE_ENV: &str = "RUSTA_CAPACITY_TEST_COOKIE";
const COOKIE: &str = "session=synthetic-capacity-demo";
const REQUEST_PATH: &str = "/synthetic-capacity-demo";

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let root = target.join("capacity-test-fixtures");
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.recursive(true).create(&root).unwrap();
        assert!(fs::symlink_metadata(&root).unwrap().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&root, &target] {
                assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o022, 0);
            }
        }
        let root = root.canonicalize().unwrap();
        let path = root.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        directory.recursive(false).create(&path).unwrap();
        let path = path.canonicalize().unwrap();
        assert_eq!(path.parent(), Some(root.as_path()));
        Self(path)
    }

    fn write_plan(&self, name: &str, plan: &Value) -> PathBuf {
        let path = self.0.join(name);
        serde_json::to_writer(File::create_new(&path).unwrap(), plan).unwrap();
        path
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct ServiceState {
    arrivals: u64,
    serving_successes: usize,
    burst_released: Option<bool>,
}

struct SharedService {
    state: Mutex<ServiceState>,
    burst_gate: Option<Range<u64>>,
    burst_arrived: Condvar,
}

struct Service {
    url: String,
    shared: Arc<SharedService>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Service {
    fn new(burst_gate: Option<Range<u64>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new(SharedService {
            state: Mutex::new(ServiceState::default()),
            burst_gate,
            burst_arrived: Condvar::new(),
        });
        let connections = shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let mut clients = Vec::new();
            // All test offers fit below this bound, including the unattempted levels.
            while !stopping.load(Ordering::Relaxed) && clients.len() < 80 {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = connections.clone();
                        clients.push(thread::spawn(move || serve(stream, shared)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            shared,
            stop,
            worker: Some(worker),
        }
    }

    fn arrivals(&self) -> u64 {
        self.shared.state.lock().unwrap().arrivals
    }

    fn burst_released(&self) -> Option<bool> {
        self.shared.state.lock().unwrap().burst_released
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, shared: Arc<SharedService>) {
    stream.set_nonblocking(false).unwrap();
    let timeout = Some(Duration::from_secs(2));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap()).take(16 * 1024);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let expected_route = line.trim_end() == format!("GET {REQUEST_PATH} HTTP/1.1");
    let mut expected_cookie = false;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            expected_cookie |= name.eq_ignore_ascii_case("cookie") && value.trim() == COOKIE;
        }
    }
    let status = {
        let mut state = shared.state.lock().unwrap();
        let arrival_index = state.arrivals;
        state.arrivals += 1;
        let status = if !expected_route || !expected_cookie {
            401
        } else if state.serving_successes >= 2 {
            503
        } else {
            state.serving_successes += 1;
            200
        };
        if let Some(burst) = &shared.burst_gate
            && burst.contains(&arrival_index)
        {
            // Hold every reply until all six requests are outstanding. The
            // timeout releases the handlers if the runner dispatches serially.
            if state.arrivals == burst.end && state.burst_released.is_none() {
                state.burst_released = Some(true);
                shared.burst_arrived.notify_all();
            }
            let (mut state, _) = shared
                .burst_arrived
                .wait_timeout_while(state, Duration::from_secs(1), |state| {
                    state.burst_released.is_none()
                })
                .unwrap();
            if state.burst_released.is_none() {
                state.burst_released = Some(false);
                shared.burst_arrived.notify_all();
            }
        }
        status
    };

    if status == 200 {
        thread::sleep(Duration::from_millis(340));
        shared.state.lock().unwrap().serving_successes -= 1;
    } else {
        thread::sleep(Duration::from_millis(40));
    }
    let _ = stream.write_all(
        format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    );
}

fn plan() -> Value {
    json!({
        "capacity_version": 1,
        "operation": "read",
        "actor": { "headers_env": { "Cookie": COOKIE_ENV } },
        "request": { "method": "GET", "path": REQUEST_PATH, "expect_status": 200 },
        "warmup": { "duration_ms": 500, "rate_per_sec": 2 },
        "sustained": { "duration_ms": 1000, "rates_per_sec": [2, 4, 20, 24] },
        "bursts": { "sizes": [1, 2, 6, 8] },
        "max_in_flight": 8,
        "timeout_ms": 2000,
        "max_start_lag_ms": 100,
    })
}

fn capacity(plan: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusta-pitest"));
    command.args(["workload", "capacity"]).arg(plan);
    command
}

fn read_report(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "capacity command failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    serde_json::from_slice(&output.stdout).expect("JSON capacity report")
}

fn assert_trials(sequence: &Value, levels: [u64; 5], ceiling: u64) {
    assert_eq!(sequence["configured_ceiling"], ceiling);
    assert_eq!(sequence["first_failure"], levels[2]);
    assert_eq!(sequence["highest_repeatedly_passing_level"], levels[1]);
    let trials = sequence["trials"].as_array().unwrap();
    assert_eq!(trials.len(), levels.len());
    for (index, (trial, level)) in trials.iter().zip(levels).enumerate() {
        assert_eq!(trial["level"], level);
        assert_eq!(trial["configured_ceiling"], ceiling);
        assert_eq!(trial["measured"], true);
        assert_eq!(trial["passed"], index != 2);
        if index < 3 {
            assert_eq!(trial["attempt"], "search");
        } else {
            assert_eq!(trial["attempt"], "confirmation");
            assert_eq!(trial["confirmation"], index - 2);
        }
    }
}

fn assert_counts_and_rates(trial: &Value) -> u64 {
    let counts = &trial["counts"];
    let number = |key| counts[key].as_u64().unwrap();
    let offered = number("offered");
    let passed = number("passed");
    assert_eq!(offered, number("started"));
    assert_eq!(offered, number("completed"));
    assert_eq!(offered, passed + number("failed"));
    assert_eq!(number("unsent"), 0);
    assert_eq!(number("no_status"), 0);
    assert_eq!(counts["by_status"]["200"], passed);
    if number("failed") > 0 {
        assert_eq!(counts["by_status"]["503"], number("failed"));
    }
    assert_eq!(trial["all_attempt_latency"]["samples"], offered);
    assert_eq!(trial["successful_request_latency"]["samples"], passed);
    let elapsed = trial["elapsed_secs"].as_f64().unwrap();
    let successful_rate = trial["successful_requests_per_sec"].as_f64().unwrap();
    assert!((successful_rate * elapsed - passed as f64).abs() < 1e-6);
    offered
}

#[test]
fn capacity_sweeps_and_confirms_with_separate_success_latency() {
    let directory = FixtureDir::new();
    let sustained_levels = [2, 4, 20, 4, 4];
    let burst_levels = [1, 2, 6, 2, 2];
    // One warmup request, five one-second sustained trials, then two bursts.
    let burst_start =
        1 + sustained_levels.iter().sum::<u64>() + burst_levels[..2].iter().sum::<u64>();
    let service = Service::new(Some(burst_start..burst_start + burst_levels[2]));
    let plan = directory.write_plan("plan.json", &plan());
    let report_path = directory.0.join("report.json");
    let output = capacity(&plan)
        .args([
            "--execute",
            "--base-url",
            &service.url,
            "--operation",
            "read",
        ])
        .arg("--output")
        .arg(&report_path)
        .env(COOKIE_ENV, COOKIE)
        .output()
        .unwrap();
    let report = read_report(&output);
    let saved: Value = serde_json::from_reader(File::open(report_path).unwrap()).unwrap();
    assert_eq!(saved, report);
    assert_eq!(report["kind"], "workload_capacity");
    assert_eq!(report["executed"], true);
    assert_eq!(report["interrupted"], false);
    assert_eq!(report["passed"], true);
    assert_eq!(report["warmup"]["measured"], false);
    assert_eq!(report["warmup"]["counts"]["offered"], 1);
    assert_eq!(report["warmup"]["counts"]["passed"], 1);
    assert_trials(&report["sustained"], sustained_levels, 24);
    assert_trials(&report["bursts"], burst_levels, 8);
    let sustained = report["sustained"]["trials"].as_array().unwrap();
    let bursts = report["bursts"]["trials"].as_array().unwrap();
    let sustained_failure = &sustained[2];
    let statuses = &sustained_failure["counts"]["by_status"];
    assert!(statuses["503"].as_u64().unwrap() > statuses["200"].as_u64().unwrap());
    let median = |key| sustained_failure[key]["p50_ms"].as_f64().unwrap();
    assert!(median("successful_request_latency") > median("all_attempt_latency") + 200.0);
    assert_eq!(service.burst_released(), Some(true));
    assert_eq!(
        bursts[2]["counts"]["by_status"],
        json!({"200": 2, "503": 4})
    );
    assert_eq!(bursts[2]["peak_client_in_flight"], burst_levels[2]);

    let measured = sustained
        .iter()
        .chain(bursts)
        .map(assert_counts_and_rates)
        .sum::<u64>();
    assert_eq!(service.arrivals(), measured + 1);
    let measured_peak = sustained
        .iter()
        .chain(bursts)
        .map(|trial| trial["peak_client_in_flight"].as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(report["peak_client_in_flight"], measured_peak);

    let serialized = String::from_utf8(output.stdout).unwrap();
    for private in [COOKIE, COOKIE_ENV, REQUEST_PATH, service.url.as_str()] {
        assert!(!serialized.contains(private));
    }
}

#[test]
fn preview_and_invalid_preflight_send_no_requests() {
    let directory = FixtureDir::new();
    let service = Service::new(None);
    let mut plan_value = plan();
    let valid = directory.write_plan("valid.json", &plan_value);
    let preview = capacity(&valid)
        .args(["--base-url", &service.url, "--operation", "read"])
        .env_remove(COOKIE_ENV)
        .output()
        .unwrap();
    let preview = read_report(&preview);
    assert_eq!(preview["executed"], false);
    assert_eq!(preview["passed"], Value::Null);
    assert_eq!(service.arrivals(), 0);

    plan_value["bursts"]["sizes"] = json!([1, 2, 9]);
    let invalid = directory.write_plan("invalid.json", &plan_value);
    for (path, operation) in [(&valid, "other"), (&invalid, "read")] {
        let output = capacity(path)
            .args([
                "--execute",
                "--base-url",
                &service.url,
                "--operation",
                operation,
            ])
            .env(COOKIE_ENV, COOKIE)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(service.arrivals(), 0);
    }
}

#[test]
fn capacity_first_failure_with_no_passing_level_has_no_confirmations() {
    let directory = FixtureDir::new();
    let service = Service::new(None);
    let mut plan_value = plan();
    plan_value["sustained"]["duration_ms"] = json!(100);
    plan_value["request"]["expect_status"] = json!(204);
    let path = directory.write_plan("fails-first.json", &plan_value);
    let output = capacity(&path)
        .args([
            "--execute",
            "--base-url",
            &service.url,
            "--operation",
            "read",
        ])
        .env(COOKIE_ENV, COOKIE)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
    for (mode, first_level) in [("sustained", 2), ("bursts", 1)] {
        let sequence = &report[mode];
        assert_eq!(sequence["first_failure"], first_level);
        assert_eq!(sequence["highest_repeatedly_passing_level"], Value::Null);
        let trials = sequence["trials"].as_array().unwrap();
        assert_eq!(trials.len(), 1);
        let trial = &trials[0];
        assert_eq!(trial["counts"]["by_status"]["200"], 1);
        assert_eq!(trial["counts"]["failed"], 1);
        assert_eq!(trial["successful_requests_per_sec"], 0.0);
        assert_eq!(trial["successful_request_latency"]["p50_ms"], Value::Null);
        assert_eq!(trial["all_attempt_latency"]["samples"], 1);
    }
    assert_eq!(service.arrivals(), 3);
}
