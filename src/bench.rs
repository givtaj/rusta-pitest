use std::{
    collections::BTreeMap,
    error::Error,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hdrhistogram::Histogram;
use reqwest::{Client, Method, Request, StatusCode, redirect, retry};
use tokio::task::JoinSet;

use crate::{
    cli::Options,
    contract::{ContractBody, ContractStats, SimulationContract},
};

type BenchError = Box<dyn Error + Send + Sync>;

pub struct Report {
    stats: Stats,
    elapsed: Duration,
    concurrency: u32,
    duration: Option<Duration>,
    request: serde_json::Value,
    workload: serde_json::Value,
    started_at_unix_ms: u64,
    finished_at_unix_ms: u64,
    contract_enabled: bool,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

enum Limit {
    Requests(u32),
    Deadline(Instant),
}

impl Limit {
    fn next_request(&mut self) -> bool {
        match self {
            Self::Requests(0) => false,
            Self::Requests(remaining) => {
                *remaining -= 1;
                true
            }
            Self::Deadline(deadline) => Instant::now() < *deadline,
        }
    }
}

struct Stats {
    succeeded: u64,
    unexpected_status: u64,
    timeouts: u64,
    transport_errors: u64,
    statuses: BTreeMap<u16, u64>,
    latency_us: Histogram<u64>,
    contract: ContractStats,
}

impl Stats {
    fn new() -> Result<Self, BenchError> {
        Ok(Self {
            succeeded: 0,
            unexpected_status: 0,
            timeouts: 0,
            transport_errors: 0,
            statuses: BTreeMap::new(),
            latency_us: Histogram::new(3)?,
            contract: ContractStats::default(),
        })
    }

    fn merge(&mut self, other: Self) -> Result<(), BenchError> {
        self.succeeded += other.succeeded;
        self.unexpected_status += other.unexpected_status;
        self.timeouts += other.timeouts;
        self.transport_errors += other.transport_errors;
        for (status, count) in other.statuses {
            *self.statuses.entry(status).or_default() += count;
        }
        self.latency_us.add(&other.latency_us)?;
        self.contract.merge(other.contract);
        Ok(())
    }
}

pub async fn run(
    method: Method,
    options: Options,
    json: Option<String>,
) -> Result<Report, BenchError> {
    run_with_contract(method, options, json, None).await
}

pub async fn run_with_contract(
    method: Method,
    options: Options,
    json: Option<String>,
    contract: Option<SimulationContract>,
) -> Result<Report, BenchError> {
    let Options { url, run: options } = options;
    let duration = options
        .duration_secs
        .map(|seconds| Duration::from_secs(u64::from(seconds)));
    let concurrency = if duration.is_some() {
        u32::from(options.concurrency)
    } else {
        u32::from(options.concurrency).min(options.requests)
    };
    // Reports omit credentials, query tokens, fragments, and request bodies.
    let mut report_url = url.clone();
    let _ = report_url.set_username("");
    let _ = report_url.set_password(None);
    report_url.set_query(None);
    report_url.set_fragment(None);
    let request_metadata = serde_json::json!({
        "method": method.as_str(),
        "url": report_url.as_str(),
        "query_redacted": url.query().is_some(),
        "body_bytes": json.as_ref().map_or(0, String::len),
    });
    let workload = serde_json::json!({
        "mode": if duration.is_some() { "duration" } else { "requests" },
        "requests": duration.is_none().then_some(options.requests),
        "duration_secs": options.duration_secs,
        "requested_concurrency": options.concurrency,
        "effective_concurrency": concurrency,
        "timeout_ms": options.timeout_ms,
        "expect_status": options.expect_status,
    });
    let client = Client::builder()
        .http1_only()
        .no_proxy()
        .redirect(redirect::Policy::none())
        .retry(retry::never())
        .timeout(Duration::from_millis(u64::from(options.timeout_ms)))
        .pool_max_idle_per_host(concurrency as usize)
        .build()?;

    let mut builder = client.request(method, url);
    if let Some(json) = json {
        builder = builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(json);
    }
    let request = builder.build()?;
    let mut stats = Stats::new()?;
    let mut workers = JoinSet::new();
    let started_at_unix_ms = unix_ms();
    let started = Instant::now();
    let deadline = duration
        .map(|duration| {
            started
                .checked_add(duration)
                .ok_or("duration exceeds the system clock range")
        })
        .transpose()?;

    for worker in 0..concurrency {
        let client = client.clone();
        let request = request
            .try_clone()
            .ok_or("the request body cannot be replayed")?;
        let limit = if let Some(deadline) = deadline {
            // All workers use the same deadline and keep one request in flight.
            Limit::Deadline(deadline)
        } else {
            // Split the request count exactly, including any remainder.
            Limit::Requests(
                options.requests / concurrency + u32::from(worker < options.requests % concurrency),
            )
        };
        workers.spawn(run_worker(
            client,
            request,
            limit,
            options.expect_status,
            contract,
        ));
    }

    while let Some(result) = workers.join_next().await {
        stats.merge(result??)?;
    }

    Ok(Report {
        stats,
        elapsed: started.elapsed(),
        concurrency,
        duration,
        request: request_metadata,
        workload,
        started_at_unix_ms,
        finished_at_unix_ms: unix_ms(),
        contract_enabled: contract.is_some(),
    })
}

async fn run_worker(
    client: Client,
    request: Request,
    mut limit: Limit,
    expected_status: Option<u16>,
    contract: Option<SimulationContract>,
) -> Result<Stats, BenchError> {
    let mut stats = Stats::new()?;
    while limit.next_request() {
        let request = request
            .try_clone()
            .ok_or("the request body cannot be replayed")?;
        let started = Instant::now();
        let result = send(&client, request, contract.is_some()).await;
        // Record every attempt, including failures, after consuming the body.
        let elapsed_us = u64::try_from(started.elapsed().as_micros())?.max(1);
        stats.latency_us.record(elapsed_us)?;

        match result {
            Ok((status, captured)) => {
                // Validate after the latency sample; throughput still includes
                // capture/validation overhead. Status accounting stays separate.
                if let (Some(contract), Some(captured)) = (contract, captured) {
                    stats.contract.record(contract.validate(status, &captured));
                }
                *stats.statuses.entry(status.as_u16()).or_default() += 1;
                let accepted = expected_status.map_or_else(
                    || status.is_success(),
                    |expected| status.as_u16() == expected,
                );
                if accepted {
                    stats.succeeded += 1;
                } else {
                    stats.unexpected_status += 1;
                }
            }
            Err(error) if error.is_timeout() => stats.timeouts += 1,
            Err(_) => stats.transport_errors += 1,
        }
    }
    Ok(stats)
}

async fn send(
    client: &Client,
    request: Request,
    capture_contract: bool,
) -> Result<(StatusCode, Option<ContractBody>), reqwest::Error> {
    let mut response = client.execute(request).await?;
    let status = response.status();
    let mut captured = capture_contract.then(|| ContractBody::new(response.headers()));
    // Always drain, including oversized bodies, to retain connection reuse and
    // full-response timing. Ordinary requests retain no body data.
    while let Some(chunk) = response.chunk().await? {
        if let Some(captured) = &mut captured {
            captured.capture(&chunk);
        }
    }
    Ok((status, captured))
}

impl Report {
    pub fn to_json(&self) -> serde_json::Value {
        let stats = &self.stats;
        let seconds = self.elapsed.as_secs_f64();
        let responses = stats.succeeded + stats.unexpected_status;
        let mut report = serde_json::json!({
            "request": self.request,
            "workload": self.workload,
            "started_at_unix_ms": self.started_at_unix_ms,
            "finished_at_unix_ms": self.finished_at_unix_ms,
            "metrics": {
                "attempted": stats.latency_us.len(),
                "succeeded": stats.succeeded,
                "unexpected_status": stats.unexpected_status,
                "timeouts": stats.timeouts,
                "transport_errors": stats.transport_errors,
                "complete_responses": responses,
                "status_counts": stats.statuses,
                "elapsed_secs": seconds,
                "complete_responses_per_sec": responses as f64 / seconds,
                "successful_responses_per_sec": stats.succeeded as f64 / seconds,
                "latency": {
                    "samples": stats.latency_us.len(),
                    "p50_us": stats.latency_us.value_at_quantile(0.50),
                    "p95_us": stats.latency_us.value_at_quantile(0.95),
                    "p99_us": stats.latency_us.value_at_quantile(0.99),
                },
            },
        });
        if self.contract_enabled {
            report["contract_validation"] = self.stats.contract.to_json();
        }
        report
    }

    pub fn passed(&self) -> bool {
        self.stats.succeeded > 0
            && self.stats.unexpected_status + self.stats.timeouts + self.stats.transport_errors == 0
            && self.stats.contract.failed == 0
    }

    pub fn print(&self) {
        let stats = &self.stats;
        let samples = stats.latency_us.len();
        let responses = stats.succeeded + stats.unexpected_status;
        let seconds = self.elapsed.as_secs_f64();

        if let Some(duration) = self.duration {
            println!("Mode: HTTP/1.1, duration, closed loop");
            println!("Request window: {} s", duration.as_secs());
            println!("In-flight requests finish under their per-request timeout.");
        } else {
            println!("Mode: HTTP/1.1, fixed request count, closed loop");
        }
        println!("Warmup: none (connection setup is included)");
        println!("Concurrency: {}", self.concurrency);
        println!("Requests: {samples}");
        println!("Succeeded: {}", stats.succeeded);
        println!("Unexpected status: {}", stats.unexpected_status);
        println!("Timeouts: {}", stats.timeouts);
        println!("Transport errors: {}", stats.transport_errors);
        if self.contract_enabled {
            stats.contract.print();
        }
        println!("Elapsed: {seconds:.3} s");
        println!(
            "Throughput: {:.2} complete responses/s",
            responses as f64 / seconds
        );
        println!(
            "Successful throughput: {:.2} responses/s",
            stats.succeeded as f64 / seconds
        );

        let statuses = stats
            .statuses
            .iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>();
        println!(
            "HTTP statuses (complete responses): {}",
            if statuses.is_empty() {
                "none".into()
            } else {
                statuses.join(", ")
            }
        );
        println!("Latency samples: {samples}");
        println!("Latency: all attempts, through full body or failure");
        println!(
            "  p50: {:.3} ms | p95: {:.3} ms | p99: {:.3} ms",
            stats.latency_us.value_at_quantile(0.50) as f64 / 1000.0,
            stats.latency_us.value_at_quantile(0.95) as f64 / 1000.0,
            stats.latency_us.value_at_quantile(0.99) as f64 / 1000.0,
        );
        if samples < 100 {
            println!("Note: fewer than 100 samples; tail percentiles are not stable.");
        }
    }
}
