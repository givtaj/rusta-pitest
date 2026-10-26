use std::error::Error;

use clap::{Args, ValueEnum};
use reqwest::{Method, Url};
use serde_json::json;

use crate::{
    bench,
    catalogue::parse_base_url,
    cli::{Options, RunOptions, parse_status},
    contract::SimulationContract,
    output::{Document, RunRecord},
};

pub const SLEEPAPI_BASE_URL: &str = "http://127.0.0.1:3000";
pub const SLEEPAPI_ROUTES: &[&str] = &[
    "/sleep/tokio",
    "/sleep/tokio-spawn",
    "/sleep/thread",
    "/sleep/park",
];

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ProfileName {
    Sleepapi,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum Scenario {
    FastSuccess,
    SlowProvider,
    DelayedFailure,
    ClientTimeout,
    RateLimited,
    InvalidRequest,
    UpstreamTimeout,
}

impl Scenario {
    pub fn defaults(self) -> (u32, u16) {
        match self {
            Self::FastSuccess => (50, 200),
            Self::SlowProvider => (2000, 200),
            Self::DelayedFailure => (800, 503),
            Self::ClientTimeout => (3000, 200),
            Self::RateLimited => (100, 429),
            Self::InvalidRequest => (50, 400),
            Self::UpstreamTimeout => (2000, 504),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::FastSuccess => "fast_success",
            Self::SlowProvider => "slow_provider",
            Self::DelayedFailure => "delayed_failure",
            Self::ClientTimeout => "client_timeout",
            Self::RateLimited => "rate_limited",
            Self::InvalidRequest => "invalid_request",
            Self::UpstreamTimeout => "upstream_timeout",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::FastSuccess => "Quick successful response",
            Self::SlowProvider => "Slow successful provider response",
            Self::DelayedFailure => "Delayed service unavailable response",
            Self::ClientTimeout => {
                "Successful response after 3 seconds; choose a shorter client timeout to test timeouts"
            }
            Self::RateLimited => "Rate limit response",
            Self::InvalidRequest => "Simulated invalid request response",
            Self::UpstreamTimeout => "Simulated upstream gateway timeout response",
        }
    }
}

#[derive(Args)]
pub struct ProfileOptions {
    /// Benchmark profile to run
    #[arg(value_enum)]
    pub profile: ProfileName,
    /// API origin; no path, credentials, query or fragment
    #[arg(long, default_value = SLEEPAPI_BASE_URL, value_parser = parse_base_url)]
    pub base_url: Url,
    /// Requests per endpoint (four endpoints are run sequentially)
    #[arg(short = 'n', long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    pub requests: u32,
    /// Request window in seconds per endpoint; finish in-flight requests
    #[arg(long, conflicts_with = "requests", value_parser = clap::value_parser!(u32).range(1..))]
    pub duration_secs: Option<u32>,
    /// Maximum concurrent requests within each endpoint
    #[arg(short = 'c', long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,
    /// Per-request timeout including the full body; timeouts fail the profile
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u32).range(1..))]
    pub timeout_ms: u32,
    /// Required response status; defaults to the configured scenario/status
    #[arg(long, value_parser = parse_status)]
    pub expect_status: Option<u16>,
    /// Apply one named scenario to all four endpoints
    #[arg(long, value_enum)]
    pub scenario: Option<Scenario>,
    /// Server delay; defaults to 400 ms, or the scenario's delay
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=3_600_000))]
    pub delay_ms: Option<u32>,
    /// Server response status; defaults to 200, conflicts with --scenario
    #[arg(long, conflicts_with = "scenario", value_parser = parse_response_status)]
    pub status_code: Option<u16>,
    /// Assert every completed SleepAPI response's JSON contract and Retry-After
    #[arg(long)]
    pub validate_contract: bool,
    /// Print all requests and settings without connecting
    #[arg(long)]
    pub dry_run: bool,
}

pub fn parse_response_status(input: &str) -> Result<u16, String> {
    input
        .parse::<u16>()
        .ok()
        .filter(|status| (200..600).contains(status) && !matches!(status, 204 | 205 | 206 | 226 | 304 | 407 | 416 | 426))
        .ok_or_else(|| {
            "expected a SleepAPI status from 200 to 599, excluding 204, 205, 206, 226, 304, 407, 416 and 426".into()
        })
}

pub async fn run(options: ProfileOptions) -> Result<Option<Document>, Box<dyn Error>> {
    // The wizard constructs these options directly, so keep the network entry
    // point safe even when clap did not perform validation.
    parse_base_url(options.base_url.as_str())?;
    if options.requests == 0
        || options.concurrency == 0
        || options.timeout_ms == 0
        || options.duration_secs == Some(0)
    {
        return Err("request count, concurrency, timeout and duration must be positive".into());
    }
    if options.delay_ms.is_some_and(|delay| delay > 3_600_000) {
        return Err("SleepAPI delay must be between 0 and 3600000 ms".into());
    }
    if options.scenario.is_some() && options.status_code.is_some() {
        return Err("--scenario cannot be combined with --status-code".into());
    }
    if let Some(status) = options.status_code {
        parse_response_status(&status.to_string())?;
    }
    if let Some(status) = options.expect_status {
        parse_status(&status.to_string())?;
    }

    let (default_delay, default_status) = options.scenario.map_or((400, 200), Scenario::defaults);
    let delay_ms = options.delay_ms.unwrap_or(default_delay);
    let status_code = options.status_code.unwrap_or(default_status);
    let expected_status = options.expect_status.unwrap_or(status_code);
    let payload = if let Some(scenario) = options.scenario {
        let mut body = json!({ "scenario": scenario.name() });
        if let Some(delay_ms) = options.delay_ms {
            body["duration_ms"] = json!(delay_ms);
        }
        body.to_string()
    } else {
        json!({ "duration_ms": delay_ms, "status_code": status_code }).to_string()
    };

    match options.profile {
        ProfileName::Sleepapi => println!("SleepAPI benchmark profile"),
    }
    println!("Base URL: {}", options.base_url);
    println!("Four endpoints run sequentially with the same workload.");
    if let Some(scenario) = options.scenario {
        println!("Scenario: {} — {}", scenario.name(), scenario.description());
    }
    println!("Server delay: {delay_ms} ms; configured response status: {status_code}");
    println!("JSON: {payload}");
    if let Some(seconds) = options.duration_secs {
        println!("Request window per endpoint: {seconds} s (plus in-flight completion)");
    } else {
        println!("Requests per endpoint: {}", options.requests);
        println!(
            "Total planned requests: {}",
            u64::from(options.requests) * 4
        );
    }
    println!("Concurrency per endpoint: {}", options.concurrency);
    println!("Timeout per request: {} ms", options.timeout_ms);
    println!("Expected status: {expected_status}");
    println!("Timeouts count as failures; use --timeout-ms below the server delay to test them.");
    if options.validate_contract {
        println!("Contract validation: every completed response; body limit 65536 bytes.");
        println!("JSON fields and Retry-After asserted against the requested simulation.");
        println!("Status matches are counted separately; any contract failure fails the profile.");
        println!(
            "Latency sampled before JSON validation; throughput includes validation overhead."
        );
    } else {
        println!("No retries; response bodies and Retry-After headers are not asserted.");
    }
    if options.dry_run {
        for route in SLEEPAPI_ROUTES {
            println!("POST {}", options.base_url.join(route)?);
        }
        println!("Dry run: no requests sent.");
        return Ok(None);
    }

    let mut results = Vec::with_capacity(SLEEPAPI_ROUTES.len());
    for route in SLEEPAPI_ROUTES {
        let url = options.base_url.join(route)?;
        println!("\nPOST {url}");
        let run = RunOptions {
            requests: options.requests,
            duration_secs: options.duration_secs,
            concurrency: options.concurrency,
            timeout_ms: options.timeout_ms,
            expect_status: Some(expected_status),
        };
        let contract = if options.validate_contract {
            Some(
                SimulationContract::for_route(
                    route,
                    delay_ms,
                    status_code,
                    options.scenario.map(Scenario::name),
                )
                .ok_or("no simulation contract for route")?,
            )
        } else {
            None
        };
        let report = match bench::run_with_contract(
            Method::POST,
            Options { url, run },
            Some(payload.clone()),
            contract,
        )
        .await
        {
            Ok(report) => {
                report.print();
                Some(report)
            }
            Err(error) => {
                eprintln!("Benchmark error for {route}: {error}");
                None
            }
        };
        results.push(RunRecord {
            route: Some(route),
            report,
        });
    }

    println!("\nSleepAPI profile summary:");
    for result in &results {
        println!(
            "  {} {}",
            if result.passed() { "PASS" } else { "FAIL" },
            result.route.unwrap()
        );
    }
    let passed_count = results.iter().filter(|result| result.passed()).count();
    println!("Endpoints passed: {passed_count}/{}", results.len());
    Ok(Some(Document {
        kind: "profile",
        profile: Some(json!({
            "name": "sleepapi",
            "scenario": options.scenario.map(Scenario::name),
            "delay_ms": delay_ms,
            "status_code": status_code,
        })),
        runs: results,
    }))
}
