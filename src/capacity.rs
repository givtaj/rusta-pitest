//! A bounded search over fixed arrival rates and synchronized read bursts.
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use hdrhistogram::Histogram;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::Semaphore, task::JoinSet, time::Instant};

use crate::{
    cli::CapacityOptions,
    workload::{hist_json, load_plan_json, offers, runner_metadata, safe_id},
    workload_http::{self, Actor, Outcome, PreparedRequest, RequestSpec},
};

const CONFIRMATIONS: u32 = 2;
const MAX_LEVELS: usize = 12;
const MAX_TRIAL_MS: u64 = 120_000;
const MAX_RATE: u32 = 1_000;
const MAX_IN_FLIGHT: usize = 128;
const MAX_TOTAL_MS: u64 = 1_200_000;
const MAX_TOTAL_OFFERS: u64 = 100_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    capacity_version: u32,
    operation: String,
    actor: Actor,
    request: RequestSpec,
    warmup: Warmup,
    sustained: Sustained,
    bursts: Bursts,
    max_in_flight: usize,
    timeout_ms: u64,
    max_start_lag_ms: u64,
    max_success_p95_ms: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Warmup {
    duration_ms: u64,
    rate_per_sec: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sustained {
    duration_ms: u64,
    rates_per_sec: Vec<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bursts {
    sizes: Vec<u32>,
}

impl Plan {
    fn validate(&self) -> Result<(), String> {
        if self.capacity_version != 1 {
            return Err("unsupported_capacity_version".into());
        }
        if !safe_id(&self.operation)
            || !matches!(self.request.method.as_str(), "GET" | "HEAD")
            || self.request.idempotency_key_env.is_some()
            || self
                .request
                .expect_status
                .is_some_and(|status| !(200..300).contains(&status))
        {
            return Err("capacity_requires_a_named_positive_read".into());
        }
        if !(1..=MAX_IN_FLIGHT).contains(&self.max_in_flight)
            || !(1..=10_000).contains(&self.timeout_ms)
            || !(1..=1_000).contains(&self.max_start_lag_ms)
            || !(1..=MAX_TRIAL_MS).contains(&self.warmup.duration_ms)
            || !(1..=MAX_TRIAL_MS).contains(&self.sustained.duration_ms)
            || !(1..=MAX_RATE).contains(&self.warmup.rate_per_sec)
            || self
                .max_success_p95_ms
                .is_some_and(|maximum| maximum <= 0.0)
        {
            return Err("capacity_limits_invalid".into());
        }
        if !valid_levels(&self.sustained.rates_per_sec, MAX_RATE)
            || !valid_levels(&self.bursts.sizes, self.max_in_flight as u32)
        {
            return Err("capacity_levels_must_increase_within_bounds".into());
        }

        let rates = &self.sustained.rates_per_sec;
        let bursts = &self.bursts.sizes;
        let rate_ceiling = *rates.last().expect("nonempty validated rates");
        let burst_ceiling = *bursts.last().expect("nonempty validated bursts");
        let repeats = u64::from(CONFIRMATIONS);
        let total_offers = offers(self.warmup.duration_ms, self.warmup.rate_per_sec)
            + rates
                .iter()
                .map(|rate| offers(self.sustained.duration_ms, *rate))
                .sum::<u64>()
            + repeats * offers(self.sustained.duration_ms, rate_ceiling)
            + bursts.iter().map(|size| u64::from(*size)).sum::<u64>()
            + repeats * u64::from(burst_ceiling);
        let total_ms = self.warmup.duration_ms
            + self.sustained.duration_ms * (rates.len() as u64 + repeats)
            + self.timeout_ms * (1 + rates.len() as u64 + bursts.len() as u64 + 2 * repeats);
        if total_offers > MAX_TOTAL_OFFERS || total_ms > MAX_TOTAL_MS {
            return Err("capacity_total_budget_exceeded".into());
        }
        Ok(())
    }
}

fn valid_levels(levels: &[u32], limit: u32) -> bool {
    (1..=MAX_LEVELS).contains(&levels.len())
        && levels[0] > 0
        && levels[levels.len() - 1] <= limit
        && levels.windows(2).all(|pair| pair[0] < pair[1])
}

#[derive(Clone, Copy)]
enum Schedule {
    FixedRate { duration_ms: u64 },
    Burst,
}

struct Latency {
    histogram: Histogram<u64>,
    maximum_us: u64,
}

impl Latency {
    fn new() -> Self {
        Self {
            histogram: Histogram::new(3).expect("valid precision"),
            maximum_us: 0,
        }
    }

    fn record(&mut self, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        self.histogram.record(us).expect("auto-resizing histogram");
        self.maximum_us = self.maximum_us.max(us);
    }

    fn json(&self) -> Value {
        hist_json(&self.histogram, self.maximum_us)
    }
}

#[derive(Default)]
struct Activity {
    current: AtomicUsize,
    peak: AtomicUsize,
}

struct InFlight<'a>(&'a Activity);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.current.fetch_sub(1, Relaxed);
    }
}

enum Attempt {
    Completed {
        outcome: Outcome,
        started: Instant,
        finished: Instant,
    },
    Late,
}

struct Trial {
    level: u32,
    offered: u64,
    started: u64,
    successful: u64,
    unsent: u64,
    no_status: u64,
    statuses: BTreeMap<u16, u64>,
    reasons: BTreeMap<&'static str, u64>,
    all_latency: Latency,
    success_latency: Latency,
    peak: usize,
    elapsed: Duration,
    interrupted: bool,
    latency_gate_passed: Option<bool>,
}

impl Trial {
    fn new(level: u32, offered: u64) -> Self {
        Self {
            level,
            offered,
            started: 0,
            successful: 0,
            unsent: 0,
            no_status: 0,
            statuses: BTreeMap::new(),
            reasons: BTreeMap::new(),
            all_latency: Latency::new(),
            success_latency: Latency::new(),
            peak: 0,
            elapsed: Duration::ZERO,
            interrupted: false,
            latency_gate_passed: None,
        }
    }

    fn unsent(&mut self, count: u64, reason: &'static str) {
        if count > 0 {
            self.unsent += count;
            *self.reasons.entry(reason).or_default() += count;
        }
    }

    fn record(&mut self, outcome: Outcome, elapsed: Duration) {
        self.started += 1;
        self.all_latency.record(elapsed);
        if let Some(code) = outcome.status {
            *self.statuses.entry(code).or_default() += 1;
        } else {
            self.no_status += 1;
        }
        if outcome.passed {
            self.successful += 1;
            self.success_latency.record(elapsed);
        } else {
            *self.reasons.entry(outcome.reason).or_default() += 1;
        }
    }

    fn passed(&self) -> bool {
        !self.interrupted
            && self.successful == self.offered
            && self.latency_gate_passed.unwrap_or(true)
    }

    fn json(&self, phase: &'static str, confirmation: Option<u32>, ceiling: u32) -> Value {
        json!({
            "level": self.level, "configured_ceiling": ceiling, "attempt": phase,
            "confirmation": confirmation, "measured": phase != "warmup",
            "passed": self.passed(), "interrupted": self.interrupted,
            "success_p95_gate_passed": self.latency_gate_passed,
            "elapsed_secs": self.elapsed.as_secs_f64(), "peak_client_in_flight": self.peak,
            "counts": {
                "offered": self.offered, "started": self.started, "completed": self.started,
                "passed": self.successful, "failed": self.started - self.successful,
                "unsent": self.unsent, "reasons": self.reasons,
                "by_status": self.statuses, "no_status": self.no_status,
            },
            "successful_requests_per_sec": self.successful as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON),
            "successful_request_latency": self.success_latency.json(),
            "all_attempt_latency": self.all_latency.json(),
        })
    }
}

struct Runner<'a> {
    plan: &'a Plan,
    client: Client,
    request: Arc<PreparedRequest>,
    interrupt: std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>,
    interrupted: bool,
}

impl Runner<'_> {
    fn spawn(
        &self,
        tasks: &mut JoinSet<Attempt>,
        activity: &Arc<Activity>,
        gate: Option<Arc<Semaphore>>,
        latest_start: Option<Instant>,
    ) {
        let client = self.client.clone();
        let request = self.request.clone();
        let activity = activity.clone();
        tasks.spawn(async move {
            if let Some(gate) = gate {
                gate.acquire()
                    .await
                    .expect("start gate remains open")
                    .forget();
            }
            let started = Instant::now();
            if latest_start.is_some_and(|deadline| started >= deadline) {
                return Attempt::Late;
            }
            let current = activity.current.fetch_add(1, Relaxed) + 1;
            let _in_flight = InFlight(&activity);
            activity.peak.fetch_max(current, Relaxed);
            let outcome = workload_http::execute(&client, &request).await;
            Attempt::Completed {
                outcome,
                started,
                finished: Instant::now(),
            }
        });
    }

    async fn trial(
        &mut self,
        schedule: Schedule,
        level: u32,
        measured: bool,
    ) -> Result<Trial, String> {
        let origin = Instant::now();
        let mut tasks = JoinSet::new();
        let activity = Arc::new(Activity::default());
        let (offered, end, mut next) = match schedule {
            Schedule::FixedRate { duration_ms } => (
                offers(duration_ms, level),
                origin + Duration::from_millis(duration_ms),
                0,
            ),
            Schedule::Burst => {
                let gate = Arc::new(Semaphore::new(0));
                for _ in 0..level {
                    self.spawn(&mut tasks, &activity, Some(gate.clone()), None);
                }
                tokio::select! {
                    biased;
                    result = &mut self.interrupt => {
                        result.map_err(|_| "capacity_interrupt_handler_unavailable")?;
                        self.interrupted = true;
                        tasks.shutdown().await;
                        let mut trial = Trial::new(level, u64::from(level));
                        trial.interrupted = true;
                        trial.unsent(trial.offered, "run_interrupted");
                        return Ok(trial);
                    }
                    // All requests have been admitted before the shared gate opens.
                    _ = std::future::ready(()) => gate.add_permits(level as usize),
                }
                (u64::from(level), origin, u64::from(level))
            }
        };
        let offer_time =
            |index: u64| origin + Duration::from_nanos(index * 1_000_000_000 / u64::from(level));
        let mut trial = Trial::new(level, offered);
        let mut window_open = matches!(schedule, Schedule::FixedRate { .. });
        let mut window_end = end;
        let mut last_finished = origin;

        while window_open || !tasks.is_empty() {
            let wake = if next < offered {
                offer_time(next).min(end)
            } else {
                end
            };
            tokio::select! {
                biased;
                result = &mut self.interrupt, if !self.interrupted => {
                    result.map_err(|_| "capacity_interrupt_handler_unavailable")?;
                    self.interrupted = true;
                    if window_open {
                        window_end = Instant::now().min(end);
                        trial.unsent(offered - next, "run_interrupted");
                        window_open = false;
                    }
                }
                result = tasks.join_next(), if !tasks.is_empty() => {
                    match result.expect("pending task").map_err(|_| "capacity_task_failed")? {
                        Attempt::Completed { outcome, started, finished } => {
                            last_finished = last_finished.max(finished);
                            trial.record(outcome, finished - started);
                        }
                        Attempt::Late => trial.unsent(1, "scheduler_late"),
                    }
                }
                _ = async {
                    if wake > Instant::now() {
                        tokio::time::sleep_until(wake).await;
                    }
                }, if window_open => {
                    let now = Instant::now();
                    if now >= end {
                        trial.unsent(offered - next, "scheduler_late");
                        window_open = false;
                        continue;
                    }
                    let lag = Duration::from_millis(self.plan.max_start_lag_ms);
                    // One nanosecond makes the configured lag allowance inclusive.
                    let expiry = (wake + lag + Duration::from_nanos(1)).min(end);
                    let expiry = if next + 1 < offered { expiry.min(offer_time(next + 1)) } else { expiry };
                    if now >= expiry {
                        trial.unsent(1, "scheduler_late");
                    } else if tasks.len() >= self.plan.max_in_flight {
                        trial.unsent(1, "capacity");
                    } else {
                        self.spawn(&mut tasks, &activity, None, Some(expiry));
                    }
                    next += 1;
                }
            }
        }
        trial.interrupted = self.interrupted;
        trial.elapsed = window_end.max(last_finished) - origin;
        trial.peak = activity.peak.load(Relaxed);
        if measured {
            trial.latency_gate_passed = self.plan.max_success_p95_ms.map(|maximum| {
                !trial.success_latency.histogram.is_empty()
                    && trial.success_latency.histogram.value_at_quantile(0.95) as f64 / 1000.0
                        <= maximum
            });
        }
        Ok(trial)
    }

    async fn sweep(&mut self, schedule: Schedule, levels: &[u32]) -> Result<Sweep, String> {
        let ceiling = *levels.last().expect("validated levels");
        let mut sweep = Sweep {
            ceiling,
            first_failure: None,
            last_pass: None,
            repeated: None,
            peak: 0,
            trials: Vec::new(),
        };
        for &level in levels {
            if self.interrupted {
                break;
            }
            let trial = self.trial(schedule, level, true).await?;
            sweep.record(&trial, "search", None);
            if trial.interrupted {
                break;
            }
            if !trial.passed() {
                sweep.first_failure = Some(level);
                break;
            }
            sweep.last_pass = Some(level);
        }
        if let Some(level) = sweep.last_pass {
            let mut confirmed = true;
            for number in 1..=CONFIRMATIONS {
                if self.interrupted {
                    confirmed = false;
                    break;
                }
                let trial = self.trial(schedule, level, true).await?;
                confirmed &= trial.passed();
                sweep.record(&trial, "confirmation", Some(number));
            }
            sweep.repeated = confirmed.then_some(level);
        }
        Ok(sweep)
    }
}

struct Sweep {
    ceiling: u32,
    first_failure: Option<u32>,
    last_pass: Option<u32>,
    repeated: Option<u32>,
    peak: usize,
    trials: Vec<Value>,
}

impl Sweep {
    fn record(&mut self, trial: &Trial, phase: &'static str, number: Option<u32>) {
        self.peak = self.peak.max(trial.peak);
        self.trials.push(trial.json(phase, number, self.ceiling));
    }

    fn json(&self) -> Value {
        json!({
            "configured_ceiling": self.ceiling, "first_failure": self.first_failure,
            "last_passing_search_level": self.last_pass,
            "highest_repeatedly_passing_level": self.repeated,
            "peak_client_in_flight": self.peak, "trials": self.trials,
        })
    }
}

pub async fn run(options: CapacityOptions) -> Result<Value, String> {
    let plan: Plan = serde_json::from_value(load_plan_json(&options.plan)?)
        .map_err(|_| "invalid_capacity_structure")?;
    plan.validate()?;
    if options
        .operation
        .as_ref()
        .is_some_and(|id| id != &plan.operation)
    {
        return Err("capacity_operation_selection_invalid".into());
    }
    let ready = options.base_url.is_some() && options.operation.is_some();
    if options.execute && !ready {
        return Err("execution_requires_explicit_base_url_and_operation".into());
    }
    let placeholder = Url::parse("http://preview.invalid/").expect("static URL");
    let request = workload_http::prepare(
        options.base_url.as_ref().unwrap_or(&placeholder),
        &plan.actor,
        &plan.request,
        Path::new("."),
        options.execute,
        false,
    )?;

    let mut report = json!({
        "schema_version": 1, "kind": "workload_capacity", "capacity_version": 1,
        "runner": runner_metadata(), "mode": if options.execute { "execute" } else { "preview" },
        "executed": options.execute, "passed": null, "ready": ready, "operation": plan.operation,
        "measurement": {
            "warmup": {"duration_ms": plan.warmup.duration_ms, "rate_per_sec": plan.warmup.rate_per_sec, "measured": false},
            "sustained": {"duration_ms": plan.sustained.duration_ms, "rates_per_sec": plan.sustained.rates_per_sec},
            "bursts": {"sizes": plan.bursts.sizes}, "confirmations": CONFIRMATIONS,
            "max_in_flight": plan.max_in_flight, "timeout_ms": plan.timeout_ms,
            "max_start_lag_ms": plan.max_start_lag_ms, "max_success_p95_ms": plan.max_success_p95_ms,
            "success_criteria": "all_offers_pass_and_optional_success_p95",
            "latency_scope": "request_start_through_full_response_and_assertions_or_failure",
            "successful_rate_denominator": "elapsed_including_drain",
            "peak_scope": "client_requests_from_start_until_terminal_outcome",
        },
    });
    if !options.execute {
        return Ok(report);
    }
    let mut runner = Runner {
        plan: &plan,
        client: workload_http::client(plan.timeout_ms)?,
        request: Arc::new(request),
        interrupt: Box::pin(tokio::signal::ctrl_c()),
        interrupted: false,
    };
    let warmup = runner
        .trial(
            Schedule::FixedRate {
                duration_ms: plan.warmup.duration_ms,
            },
            plan.warmup.rate_per_sec,
            false,
        )
        .await?;
    let sustained = runner
        .sweep(
            Schedule::FixedRate {
                duration_ms: plan.sustained.duration_ms,
            },
            &plan.sustained.rates_per_sec,
        )
        .await?;
    let bursts = runner.sweep(Schedule::Burst, &plan.bursts.sizes).await?;
    report["warmup"] = warmup.json("warmup", None, plan.warmup.rate_per_sec);
    report["sustained"] = sustained.json();
    report["bursts"] = bursts.json();
    report["interrupted"] = json!(runner.interrupted);
    report["peak_client_in_flight"] = json!(sustained.peak.max(bursts.peak));
    report["passed"] =
        json!(!runner.interrupted && sustained.repeated.is_some() && bursts.repeated.is_some());
    Ok(report)
}
