//! Reviewed, bounded open-loop workloads. Existing benchmark/suite semantics stay separate.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    future::Future,
    io::Read,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hdrhistogram::Histogram;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{task::JoinSet, time::Instant};

use crate::workload_http::{self, Actor, Outcome, PreparedRequest, RequestSpec};

const MAX_PLAN_BYTES: u64 = 1024 * 1024;
const MAX_OFFERS: u64 = 1_000_000;
const MAX_SERIES: usize = 512;

pub struct Options {
    pub plan: PathBuf,
    pub execute: bool,
    pub base_url: Option<Url>,
    pub operations: Vec<String>,
    pub allow_writes: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    workload_version: u32,
    actors: BTreeMap<String, Actor>,
    requests: BTreeMap<String, RequestSpec>,
    groups: BTreeMap<String, Group>,
    phases: Vec<Phase>,
    #[serde(default)]
    setup: Vec<SetupStep>,
    #[serde(default = "default_capacity")]
    max_in_flight: usize,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_lag")]
    max_start_lag_ms: u64,
    #[serde(default = "default_timeout")]
    drain_timeout_ms: u64,
    #[serde(default)]
    thresholds: Vec<Threshold>,
    #[serde(default)]
    evidence: EvidenceOptions,
}

fn default_capacity() -> usize {
    16
}
fn default_timeout() -> u64 {
    5000
}
fn default_lag() -> u64 {
    20
}
fn default_records() -> usize {
    10_000
}
fn default_bytes() -> usize {
    2 * 1024 * 1024
}
fn default_samples() -> u64 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    actors: Vec<String>,
    max_in_flight: usize,
    #[serde(default)]
    requests: Vec<String>,
    #[serde(default)]
    sequence: Vec<Step>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    request: String,
    replay_of: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupStep {
    actor: String,
    request: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Phase {
    name: String,
    duration_ms: u64,
    rates: BTreeMap<String, u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Threshold {
    metric: String,
    maximum: f64,
    #[serde(default = "default_samples")]
    min_samples: u64,
    request: Option<String>,
    group: Option<String>,
    phase: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceOptions {
    #[serde(default = "default_records")]
    max_records: usize,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
}

impl Default for EvidenceOptions {
    fn default() -> Self {
        Self {
            max_records: default_records(),
            max_bytes: default_bytes(),
        }
    }
}

#[derive(Clone, Default)]
struct Counts {
    offered: u64,
    started: u64,
    completed: u64,
    passed: u64,
    failed: u64,
    cancelled: u64,
    unsent: u64,
    complete_http_responses: u64,
    reasons: BTreeMap<String, u64>,
}

impl Counts {
    fn merge(&mut self, other: &Self) {
        self.offered += other.offered;
        self.started += other.started;
        self.completed += other.completed;
        self.passed += other.passed;
        self.failed += other.failed;
        self.cancelled += other.cancelled;
        self.unsent += other.unsent;
        self.complete_http_responses += other.complete_http_responses;
        for (key, count) in &other.reasons {
            *self.reasons.entry(key.clone()).or_default() += count;
        }
    }

    fn json(&self) -> Value {
        json!({
            "offered":self.offered,"started":self.started,"completed":self.completed,
            "passed":self.passed,"failed":self.failed,"cancelled":self.cancelled,
            "unsent":self.unsent,"in_flight":self.started-self.completed-self.cancelled,
            "complete_http_responses":self.complete_http_responses,"reasons":self.reasons,
        })
    }
}

struct Metrics {
    counts: Counts,
    latency: Histogram<u64>,
    lag: Histogram<u64>,
    scheduled_latency: Histogram<u64>,
    latency_max_us: u64,
    lag_max_us: u64,
    scheduled_max_us: u64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            counts: Counts::default(),
            latency: Histogram::new(3).expect("valid precision"),
            lag: Histogram::new(3).expect("valid precision"),
            scheduled_latency: Histogram::new(3).expect("valid precision"),
            latency_max_us: 0,
            lag_max_us: 0,
            scheduled_max_us: 0,
        }
    }
    fn merge(&mut self, other: &Self) {
        self.counts.merge(&other.counts);
        self.latency
            .add(&other.latency)
            .expect("matching histograms");
        self.lag.add(&other.lag).expect("matching histograms");
        self.scheduled_latency
            .add(&other.scheduled_latency)
            .expect("matching histograms");
        self.latency_max_us = self.latency_max_us.max(other.latency_max_us);
        self.lag_max_us = self.lag_max_us.max(other.lag_max_us);
        self.scheduled_max_us = self.scheduled_max_us.max(other.scheduled_max_us);
    }
    fn json(&self) -> Value {
        json!({"counts":self.counts.json(),"latency":hist_json(&self.latency,self.latency_max_us),
            "start_lag":hist_json(&self.lag,self.lag_max_us),"scheduled_to_terminal":hist_json(&self.scheduled_latency,self.scheduled_max_us)})
    }
}

fn hist_json(hist: &Histogram<u64>, maximum_us: u64) -> Value {
    if hist.is_empty() {
        return json!({"samples":0,"p50_ms":null,"p95_ms":null,"p99_ms":null,"max_ms":null});
    }
    json!({"samples":hist.len(),"p50_ms":hist.value_at_quantile(0.5) as f64/1000.0,
        "p95_ms":hist.value_at_quantile(0.95) as f64/1000.0,
        "p99_ms":hist.value_at_quantile(0.99) as f64/1000.0,"max_ms":maximum_us as f64/1000.0})
}

type SeriesKey = (String, String, String); // request, group, phase
type Prepared = BTreeMap<(String, String), Arc<PreparedRequest>>;

struct Capture {
    records: Vec<Value>,
    bytes: usize,
    dropped: u64,
    max_records: usize,
    max_bytes: usize,
    exhausted: bool,
}

impl Capture {
    fn new(options: &EvidenceOptions) -> Self {
        Self {
            records: Vec::new(),
            bytes: 0,
            dropped: 0,
            max_records: options.max_records,
            max_bytes: options.max_bytes,
            exhausted: false,
        }
    }
    fn can_record(&self) -> bool {
        !self.exhausted && self.records.len() < self.max_records && self.bytes < self.max_bytes
    }
    fn record(&mut self, value: Value) {
        if !self.can_record() {
            self.dropped += 1;
            return;
        }
        let bytes = serde_json::to_vec(&value).expect("JSON record").len() + 1;
        if self.records.len() >= self.max_records
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            self.dropped += 1;
            self.exhausted = true;
        } else {
            self.bytes += bytes;
            self.records.push(value);
        }
    }
    fn json(self, interrupted: bool) -> Value {
        json!({"enabled":self.max_records>0&&self.max_bytes>0,"complete":!interrupted&&self.dropped==0,
            "captured_records":self.records.len(),"dropped_records":self.dropped,
            "encoded_records_bytes":self.bytes,"byte_scope":"compact_records_plus_one_delimiter_each_excluding_report_envelope","max_records":self.max_records,"max_bytes":self.max_bytes,
            "retention":"prefix_until_first_limit","records":self.records})
    }
}

struct Cursor {
    group: String,
    phase: String,
    phase_start_ns: u64,
    rate: u32,
    next: u64,
    total: u64,
}

impl Cursor {
    fn offset_ns(&self) -> u64 {
        self.phase_start_ns
            + ((u128::from(self.next) * 1_000_000_000) / u128::from(self.rate)) as u64
    }
    fn next_offset_ns(&self) -> u64 {
        self.phase_start_ns
            + ((u128::from(self.next + 1) * 1_000_000_000) / u128::from(self.rate)) as u64
    }
}

struct Active {
    actor: String,
    group: String,
    phase: String,
    scheduled: Instant,
    actual_started: Arc<Mutex<Option<Instant>>>,
    steps: Vec<Step>,
    step: usize,
    sequence: Option<String>,
    request_id: String,
}

struct Completion {
    id: u64,
    outcome: Outcome,
    started: Instant,
    finished: Instant,
}

struct Recorder {
    origin: Instant,
    metrics: BTreeMap<SeriesKey, Metrics>,
    capture: Capture,
    next_request: u64,
}

impl Recorder {
    fn series(&mut self, request: &str, group: &str, phase: &str) -> &mut Metrics {
        self.metrics
            .entry((request.into(), group.into(), phase.into()))
            .or_insert_with(Metrics::new)
    }

    fn start(&mut self, active: &mut Active) {
        self.next_request += 1;
        active.request_id = format!("request-{}", self.next_request);
        self.series(
            &active.steps[active.step].request,
            &active.group,
            &active.phase,
        )
        .counts
        .started += 1;
    }

    fn unsent(&mut self, active: &Active, first_step: usize, reason: &str) {
        for step in first_step..active.steps.len() {
            let request = &active.steps[step].request;
            let counts = &mut self.series(request, &active.group, &active.phase).counts;
            counts.unsent += 1;
            *counts.reasons.entry(reason.into()).or_default() += 1;
            self.unsent_event(active, step, reason);
        }
    }

    fn unsent_event(&mut self, active: &Active, step: usize, reason: &str) {
        self.next_request += 1;
        if !self.capture.can_record() {
            self.capture.dropped += 1;
            return;
        }
        self.capture.record(json!({"request_id":format!("request-{}",self.next_request),
                "operation":active.steps[step].request,"actor":active.actor,"group":active.group,"phase":active.phase,
                "sequence_id":active.sequence,"step":step,"replay_of":active.steps[step].replay_of,
                "scheduled_ms":offset_ms(active.scheduled,self.origin),"started_ms":null,
                "completed_ms":null,"outcome":"unsent","reason":reason}));
    }

    /// Account future arrivals without building one object per planned request.
    /// Return the number of event objects constructed, for bounded-work tests.
    fn bulk_unsent(&mut self, plan: &Plan, cursor: &mut Cursor, reason: &str) -> u64 {
        let group = &plan.groups[&cursor.group];
        let remaining = cursor.total - cursor.next;
        if remaining == 0 {
            return 0;
        }
        let request_count = remaining * group.sequence.len().max(1) as u64;
        if group.sequence.is_empty() {
            let length = group.requests.len() as u64;
            for (index, request) in group.requests.iter().enumerate() {
                let count = cycle_count(cursor.total, length, index as u64)
                    - cycle_count(cursor.next, length, index as u64);
                let counts = &mut self.series(request, &cursor.group, &cursor.phase).counts;
                counts.unsent += count;
                *counts.reasons.entry(reason.into()).or_default() += count;
            }
        } else {
            for step in &group.sequence {
                let counts = &mut self
                    .series(&step.request, &cursor.group, &cursor.phase)
                    .counts;
                counts.unsent += remaining;
                *counts.reasons.entry(reason.into()).or_default() += remaining;
            }
        }
        let mut materialized = 0u64;
        while cursor.next < cursor.total && self.capture.can_record() {
            let request = arrival(plan, cursor, self.origin);
            for step in 0..request.steps.len() {
                if !self.capture.can_record() {
                    break;
                }
                self.unsent_event(&request, step, reason);
                materialized += 1;
            }
            cursor.next += 1;
        }
        let omitted = request_count - materialized;
        self.next_request += omitted;
        self.capture.dropped += omitted;
        cursor.next = cursor.total;
        materialized
    }

    fn finish(&mut self, active: &Active, outcome: &Outcome, started: Instant, finished: Instant) {
        let metrics = self.series(
            &active.steps[active.step].request,
            &active.group,
            &active.phase,
        );
        metrics.counts.completed += 1;
        if outcome.passed {
            metrics.counts.passed += 1;
        } else {
            metrics.counts.failed += 1;
        }
        if outcome.response_complete {
            metrics.counts.complete_http_responses += 1;
        }
        *metrics
            .counts
            .reasons
            .entry(outcome.reason.into())
            .or_default() += 1;
        metrics
            .latency
            .record(micros(finished.saturating_duration_since(started)))
            .expect("bounded duration");
        metrics
            .lag
            .record(micros(started.saturating_duration_since(active.scheduled)))
            .expect("bounded duration");
        metrics
            .scheduled_latency
            .record(micros(finished.saturating_duration_since(active.scheduled)))
            .expect("bounded duration");
        metrics.latency_max_us = metrics
            .latency_max_us
            .max(micros(finished.saturating_duration_since(started)));
        metrics.lag_max_us = metrics
            .lag_max_us
            .max(micros(started.saturating_duration_since(active.scheduled)));
        metrics.scheduled_max_us = metrics
            .scheduled_max_us
            .max(micros(finished.saturating_duration_since(active.scheduled)));
        self.capture.record(json!({"request_id":active.request_id,"operation":active.steps[active.step].request,
            "actor":active.actor,"group":active.group,"phase":active.phase,"sequence_id":active.sequence,
            "step":active.step,"replay_of":active.steps[active.step].replay_of,
            "scheduled_ms":offset_ms(active.scheduled,self.origin),"started_ms":offset_ms(started,self.origin),
            "completed_ms":offset_ms(finished,self.origin),"outcome":if outcome.passed {"passed"} else {"failed"},
            "status":outcome.status,"reason":outcome.reason}));
    }

    fn cancel(&mut self, active: &Active) -> bool {
        let actual_started = *active.actual_started.lock().expect("start timestamp lock");
        let metrics = self.series(
            &active.steps[active.step].request,
            &active.group,
            &active.phase,
        );
        if actual_started.is_none() {
            // Dispatch is provisional until the HTTP task actually starts polling.
            metrics.counts.started -= 1;
            self.unsent(active, active.step, "run_cancelled");
            return false;
        }
        metrics.counts.cancelled += 1;
        *metrics
            .counts
            .reasons
            .entry("drain_timeout".into())
            .or_default() += 1;
        self.capture.record(json!({"request_id":active.request_id,"operation":active.steps[active.step].request,
            "actor":active.actor,"group":active.group,"phase":active.phase,"sequence_id":active.sequence,
            "step":active.step,"replay_of":active.steps[active.step].replay_of,
            "scheduled_ms":offset_ms(active.scheduled,self.origin),"started_ms":actual_started.map(|at|offset_ms(at,self.origin)),
            "completed_ms":offset_ms(Instant::now(),self.origin),"outcome":"cancelled","reason":"drain_timeout"}));
        true
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros())
        .unwrap_or(u64::MAX)
        .max(1)
}
fn offset_ms(at: Instant, origin: Instant) -> f64 {
    at.saturating_duration_since(origin).as_secs_f64() * 1000.0
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn offers(duration_ms: u64, rate: u32) -> u64 {
    (duration_ms * u64::from(rate)).div_ceil(1000)
}

fn cycle_count(end: u64, length: u64, index: u64) -> u64 {
    if end <= index {
        0
    } else {
        (end - 1 - index) / length + 1
    }
}

fn load(path: &std::path::Path) -> Result<Plan, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| "cannot_open_workload")?
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot_read_workload")?;
    let value = crate::strict_json::parse_json_with_limit(&bytes, MAX_PLAN_BYTES as usize)?;
    serde_json::from_value(value).map_err(|_| "invalid_workload_structure".into())
}

fn validate(plan: &Plan) -> Result<(), String> {
    if plan.workload_version != 1 {
        return Err("unsupported_workload_version".into());
    }
    if plan.actors.is_empty()
        || plan.actors.len() > 128
        || plan.requests.is_empty()
        || plan.requests.len() > 128
        || plan.groups.is_empty()
        || plan.groups.len() > 32
        || plan.phases.is_empty()
        || plan.phases.len() > 16
        || plan.setup.len() > 64
        || plan.thresholds.len() > 128
    {
        return Err("workload_collection_limit".into());
    }
    if plan.max_in_flight == 0
        || plan.max_in_flight > 256
        || plan.timeout_ms == 0
        || plan.timeout_ms > 300_000
        || plan.drain_timeout_ms == 0
        || plan.drain_timeout_ms > 300_000
        || plan.max_start_lag_ms > 60_000
        || plan.evidence.max_records > 100_000
        || plan.evidence.max_bytes > 64 * 1024 * 1024
    {
        return Err("workload_configuration_limit".into());
    }
    if !plan
        .actors
        .keys()
        .chain(plan.requests.keys())
        .chain(plan.groups.keys())
        .all(|id| safe_id(id))
    {
        return Err("workload_identifier_invalid".into());
    }
    let mut reserved = 0usize;
    let mut actor_groups = BTreeSet::new();
    for group in plan.groups.values() {
        if group.actors.is_empty()
            || group.actors.len() > 128
            || group.max_in_flight == 0
            || group.max_in_flight > group.actors.len()
            || group.requests.len() > 128
            || group.sequence.len() > 64
            || group.requests.is_empty() == group.sequence.is_empty()
        {
            return Err("workload_group_invalid".into());
        }
        reserved += group.max_in_flight;
        let mut unique = BTreeSet::new();
        for actor in &group.actors {
            if !plan.actors.contains_key(actor)
                || !unique.insert(actor)
                || !actor_groups.insert(actor)
            {
                return Err("actors_must_belong_to_one_group".into());
            }
        }
        for request in &group.requests {
            if !plan.requests.contains_key(request) {
                return Err("unknown_workload_request".into());
            }
        }
        for (index, step) in group.sequence.iter().enumerate() {
            let spec = plan
                .requests
                .get(&step.request)
                .ok_or("unknown_workload_request")?;
            if let Some(previous) = step.replay_of
                && (previous >= index
                    || group.sequence[previous].request != step.request
                    || spec.idempotency_key_env.is_none())
            {
                return Err(
                    "replay_requires_an_identical_prior_request_and_idempotency_key".into(),
                );
            }
        }
    }
    if reserved > plan.max_in_flight {
        return Err("group_reservations_exceed_global_capacity".into());
    }
    let mut phase_names = BTreeSet::new();
    let mut total_duration = 0u64;
    let mut total_requests = 0u64;
    let mut series_count = 0usize;
    for phase in &plan.phases {
        if !safe_id(&phase.name)
            || !phase_names.insert(&phase.name)
            || phase.duration_ms == 0
            || phase.duration_ms > 3_600_000
        {
            return Err("workload_phase_invalid".into());
        }
        total_duration += phase.duration_ms;
        for (name, rate) in &phase.rates {
            let group = plan.groups.get(name).ok_or("unknown_workload_group")?;
            if *rate > 10_000 {
                return Err("workload_rate_limit".into());
            }
            total_requests += offers(phase.duration_ms, *rate) * group.sequence.len().max(1) as u64;
            if *rate > 0 {
                let ids: BTreeSet<_> = group
                    .requests
                    .iter()
                    .chain(group.sequence.iter().map(|step| &step.request))
                    .collect();
                series_count += ids.len();
            }
        }
    }
    if total_duration > 86_400_000
        || total_requests == 0
        || total_requests > MAX_OFFERS
        || series_count > MAX_SERIES
    {
        return Err("workload_total_limit".into());
    }
    for step in &plan.setup {
        if !plan.actors.contains_key(&step.actor) || !plan.requests.contains_key(&step.request) {
            return Err("unknown_setup_binding".into());
        }
    }
    for threshold in &plan.thresholds {
        if !matches!(
            threshold.metric.as_str(),
            "p95_ms"
                | "max_ms"
                | "start_lag_p95_ms"
                | "scheduled_p95_ms"
                | "error_rate"
                | "unsent_rate"
        ) || !threshold.maximum.is_finite()
            || threshold.maximum < 0.0
            || (threshold.metric.ends_with("rate") && threshold.maximum > 1.0)
            || threshold.min_samples == 0
            || threshold.min_samples > MAX_OFFERS
            || threshold
                .request
                .as_ref()
                .is_some_and(|id| !plan.requests.contains_key(id))
            || threshold
                .group
                .as_ref()
                .is_some_and(|id| !plan.groups.contains_key(id))
            || threshold
                .phase
                .as_ref()
                .is_some_and(|id| !phase_names.contains(id))
        {
            return Err("workload_threshold_invalid".into());
        }
    }
    Ok(())
}

fn required_pairs(plan: &Plan) -> BTreeSet<(String, String)> {
    let mut pairs = BTreeSet::new();
    for group in plan.groups.values() {
        for actor in &group.actors {
            for request in group
                .requests
                .iter()
                .chain(group.sequence.iter().map(|step| &step.request))
            {
                pairs.insert((actor.clone(), request.clone()));
            }
        }
    }
    pairs.extend(
        plan.setup
            .iter()
            .map(|step| (step.actor.clone(), step.request.clone())),
    );
    pairs
}

fn prepare(plan: &Plan, options: &Options) -> Result<Prepared, String> {
    let pairs = required_pairs(plan);
    if pairs.len() > 512 {
        return Err("workload_prepared_request_limit".into());
    }
    let operations: BTreeSet<_> = options.operations.iter().cloned().collect();
    if operations.len() != options.operations.len()
        || operations.iter().any(|id| !plan.requests.contains_key(id))
    {
        return Err("workload_operation_selection_invalid".into());
    }
    if options.execute
        && (options.base_url.is_none()
            || pairs
                .iter()
                .any(|(_, request)| !operations.contains(request)))
    {
        return Err("execution_requires_explicit_base_url_and_every_referenced_operation".into());
    }
    let placeholder = Url::parse("http://preview.invalid/").expect("static URL");
    let base = options.base_url.as_ref().unwrap_or(&placeholder);
    let root = options
        .plan
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    pairs
        .into_iter()
        .map(|(actor, request)| {
            let prepared = workload_http::prepare(
                base,
                &plan.actors[&actor],
                &plan.requests[&request],
                root,
                options.execute,
                options.allow_writes || !options.execute,
            )?;
            Ok(((actor, request), Arc::new(prepared)))
        })
        .collect()
}

fn cursors(plan: &Plan) -> Vec<Cursor> {
    let mut result = Vec::new();
    let mut phase_start_ns = 0;
    for phase in &plan.phases {
        for (group, rate) in &phase.rates {
            if *rate > 0 {
                result.push(Cursor {
                    group: group.clone(),
                    phase: phase.name.clone(),
                    phase_start_ns,
                    rate: *rate,
                    next: 0,
                    total: offers(phase.duration_ms, *rate),
                });
            }
        }
        phase_start_ns += phase.duration_ms * 1_000_000;
    }
    result
}

fn initialize_metrics(plan: &Plan, cursors: &[Cursor], recorder: &mut Recorder) {
    for cursor in cursors {
        let group = &plan.groups[&cursor.group];
        if !group.sequence.is_empty() {
            for step in &group.sequence {
                recorder
                    .series(&step.request, &cursor.group, &cursor.phase)
                    .counts
                    .offered += cursor.total;
            }
        } else {
            let length = group.requests.len() as u64;
            for (index, request) in group.requests.iter().enumerate() {
                let count =
                    cursor.total / length + u64::from((index as u64) < cursor.total % length);
                recorder
                    .series(request, &cursor.group, &cursor.phase)
                    .counts
                    .offered += count;
            }
        }
    }
}

fn arrival(plan: &Plan, cursor: &Cursor, origin: Instant) -> Active {
    let group = &plan.groups[&cursor.group];
    let steps = if group.sequence.is_empty() {
        vec![Step {
            request: group.requests[cursor.next as usize % group.requests.len()].clone(),
            replay_of: None,
        }]
    } else {
        group.sequence.clone()
    };
    Active {
        actor: group.actors[cursor.next as usize % group.actors.len()].clone(),
        group: cursor.group.clone(),
        phase: cursor.phase.clone(),
        scheduled: origin + Duration::from_nanos(cursor.offset_ns()),
        actual_started: Arc::new(Mutex::new(None)),
        steps,
        step: 0,
        sequence: (!group.sequence.is_empty())
            .then(|| format!("{}.{}.{}", cursor.phase, cursor.group, cursor.next)),
        request_id: String::new(),
    }
}

type ExecuteFuture = Pin<Box<dyn Future<Output = Outcome> + Send>>;
type Executor = fn(String, Client, Arc<PreparedRequest>) -> ExecuteFuture;

fn execute_http(_group: String, client: Client, request: Arc<PreparedRequest>) -> ExecuteFuture {
    Box::pin(async move { workload_http::execute(&client, &request).await })
}

fn spawn(
    active: &mut Active,
    id: u64,
    prepared: &Prepared,
    client: &Client,
    recorder: &mut Recorder,
    tasks: &mut JoinSet<Completion>,
    executor: Executor,
) {
    recorder.start(active);
    active.actual_started = Arc::new(Mutex::new(None));
    let actual_started = active.actual_started.clone();
    let request = prepared[&(
        active.actor.clone(),
        active.steps[active.step].request.clone(),
    )]
        .clone();
    let client = client.clone();
    let group = active.group.clone();
    tasks.spawn(async move {
        let started = Instant::now();
        *actual_started.lock().expect("start timestamp lock") = Some(started);
        let outcome = executor(group, client, request).await;
        Completion {
            id,
            outcome,
            started,
            finished: Instant::now(),
        }
    });
}

fn scoped_metrics(series: &BTreeMap<SeriesKey, Metrics>, threshold: &Threshold) -> Metrics {
    let mut aggregate = Metrics::new();
    for (key, metrics) in series {
        if threshold_matches(threshold, key) {
            aggregate.merge(metrics);
        }
    }
    aggregate
}

fn threshold_matches(threshold: &Threshold, key: &SeriesKey) -> bool {
    threshold.request.as_ref().is_none_or(|id| id == &key.0)
        && threshold.group.as_ref().is_none_or(|id| id == &key.1)
        && threshold.phase.as_ref().is_none_or(|id| id == &key.2)
}

fn failures_have_allowances(
    series: &BTreeMap<SeriesKey, Metrics>,
    thresholds: &[Threshold],
) -> bool {
    series.iter().all(|(key, metrics)| {
        (metrics.counts.failed == 0
            || thresholds.iter().any(|threshold| {
                threshold.metric == "error_rate" && threshold_matches(threshold, key)
            }))
            && (metrics.counts.unsent == 0
                || thresholds.iter().any(|threshold| {
                    threshold.metric == "unsent_rate" && threshold_matches(threshold, key)
                }))
    })
}

fn evaluate(threshold: &Threshold, metrics: &Metrics) -> Value {
    let (actual, samples) = match threshold.metric.as_str() {
        "p95_ms" => (
            metrics.latency.value_at_quantile(0.95) as f64 / 1000.0,
            metrics.latency.len(),
        ),
        "max_ms" => (
            metrics.latency_max_us as f64 / 1000.0,
            metrics.latency.len(),
        ),
        "start_lag_p95_ms" => (
            metrics.lag.value_at_quantile(0.95) as f64 / 1000.0,
            metrics.lag.len(),
        ),
        "scheduled_p95_ms" => (
            metrics.scheduled_latency.value_at_quantile(0.95) as f64 / 1000.0,
            metrics.scheduled_latency.len(),
        ),
        "error_rate" => (
            metrics.counts.failed as f64 / metrics.counts.completed.max(1) as f64,
            metrics.counts.completed,
        ),
        "unsent_rate" => (
            metrics.counts.unsent as f64 / metrics.counts.offered.max(1) as f64,
            metrics.counts.offered,
        ),
        _ => unreachable!("validated metric"),
    };
    let enough = samples >= threshold.min_samples;
    json!({"metric":threshold.metric,"maximum":threshold.maximum,"min_samples":threshold.min_samples,
        "scope":{"request":threshold.request,"group":threshold.group,"phase":threshold.phase},
        "actual":if samples==0 {None} else {Some(actual)},"samples":samples,
        "passed":enough&&actual<=threshold.maximum,"reason":if !enough {"insufficient_samples"} else if actual>threshold.maximum {"maximum_exceeded"} else {"passed"}})
}

fn runner_metadata() -> Value {
    let revision = env!("RUSTA_GIT_REVISION");
    json!({
        "name":env!("CARGO_PKG_NAME"),"version":env!("CARGO_PKG_VERSION"),
        "git_revision":(!revision.is_empty()).then_some(revision),
        "source_dirty":match env!("RUSTA_SOURCE_DIRTY") { "true"=>Some(true),"false"=>Some(false),_=>None },
        "target":env!("RUSTA_BUILD_TARGET"),"rustc_version":env!("RUSTA_RUSTC_VERSION"),
        "build_profile":env!("RUSTA_BUILD_PROFILE"),
    })
}

pub async fn run(options: Options) -> Result<Value, String> {
    run_with_executor(options, execute_http).await
}

async fn run_with_executor(options: Options, executor: Executor) -> Result<Value, String> {
    let plan = load(&options.plan)?;
    validate(&plan)?;
    let prepared = prepare(&plan, &options)?;
    let required: BTreeSet<_> = required_pairs(&plan)
        .into_iter()
        .map(|(_, request)| request)
        .collect();
    let writes = required
        .iter()
        .any(|id| !matches!(plan.requests[id].method.as_str(), "GET" | "HEAD"));
    if !options.execute {
        let selected: BTreeSet<_> = options.operations.iter().cloned().collect();
        let missing: Vec<_> = required.difference(&selected).cloned().collect();
        return Ok(
            json!({"schema_version":2,"kind":"workload","workload_version":1,"mode":"preview","runner":runner_metadata(),
            "executed":false,"passed":null,"ready":options.base_url.is_some()&&(!writes||options.allow_writes)&&missing.is_empty(),
            "required_operations":required,"missing_operations":missing,"requires_write_opt_in":writes,"runtime_secrets_loaded":false,
            "planned_requests":planned_requests(&plan),"groups":plan.groups.iter().map(|(id,group)|json!({"id":id,
                "arrival_unit":if group.sequence.is_empty(){"request"}else{"sequence"},"reserved_capacity":group.max_in_flight})).collect::<Vec<_>>(),
            "phases":plan.phases.iter().map(|phase|json!({"name":phase.name,"duration_ms":phase.duration_ms,"rates":phase.rates})).collect::<Vec<_>>(),
            "setup_steps":plan.setup.len()}),
        );
    }
    let client = workload_http::client(plan.timeout_ms)?;
    let mut signal = Box::pin(tokio::signal::ctrl_c());
    let mut setup_rows = Vec::new();
    let mut setup_failed = false;
    let mut interrupted = false;
    for step in &plan.setup {
        let request = &prepared[&(step.actor.clone(), step.request.clone())];
        let outcome = tokio::select! {
            outcome=workload_http::execute(&client,request)=>Some(outcome),
            _=&mut signal=>{interrupted=true;None},
        };
        let Some(outcome) = outcome else {
            break;
        };
        setup_rows.push(json!({"actor":step.actor,"operation":step.request,"passed":outcome.passed,"status":outcome.status,"reason":outcome.reason}));
        if !outcome.passed {
            setup_failed = true;
            break;
        }
    }
    let origin = Instant::now();
    let wall_anchor = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let run_id = format!("run-{}-{}", wall_anchor, std::process::id());
    let mut recorder = Recorder {
        origin,
        metrics: BTreeMap::new(),
        capture: Capture::new(&plan.evidence),
        next_request: 0,
    };
    let mut cursors = cursors(&plan);
    initialize_metrics(&plan, &cursors, &mut recorder);
    let mut sequence_counts = Counts::default();
    for cursor in &cursors {
        if !plan.groups[&cursor.group].sequence.is_empty() {
            sequence_counts.offered += cursor.total;
        }
    }
    let end =
        origin + Duration::from_millis(plan.phases.iter().map(|phase| phase.duration_ms).sum());
    let mut stopping = setup_failed || interrupted;
    let mut drain_end = if stopping {
        Some(origin + Duration::from_millis(plan.drain_timeout_ms))
    } else {
        None
    };
    let mut active = BTreeMap::<u64, Active>::new();
    let mut group_active = BTreeMap::<String, usize>::new();
    let mut actor_active = BTreeSet::<String>::new();
    let mut tasks = JoinSet::<Completion>::new();
    let mut next_task = 0u64;
    let mut capture_incomplete = interrupted;
    loop {
        let now = Instant::now();
        if !stopping && now >= end {
            stopping = true;
            drain_end = Some(now + Duration::from_millis(plan.drain_timeout_ms));
        }
        if stopping {
            // Finish/abort active HTTP first. Future-offer bookkeeping cannot
            // postpone the drain deadline or turn completed work into cancellation.
            if active.is_empty() {
                break;
            }
            if drain_end.is_some_and(|deadline| now >= deadline) {
                tasks.abort_all();
                for request in active.values() {
                    if recorder.cancel(request) {
                        recorder.unsent(request, request.step + 1, "run_cancelled");
                    }
                    if request.sequence.is_some() {
                        sequence_counts.cancelled += 1;
                    }
                }
                capture_incomplete = true;
                active.clear();
                while tasks.join_next().await.is_some() {}
                break;
            }
        }
        let next = cursors
            .iter()
            .enumerate()
            .filter(|(_, cursor)| cursor.next < cursor.total)
            .min_by_key(|(_, cursor)| cursor.offset_ns())
            .map(|(index, cursor)| (index, origin + Duration::from_nanos(cursor.offset_ns())));
        let wake = if stopping {
            drain_end.expect("drain deadline")
        } else {
            next.map_or(end, |(_, at)| at.min(end))
        };
        tokio::select! {
            biased;
            completion=tasks.join_next(),if !tasks.is_empty()=>{
                let completion=completion.expect("pending task").map_err(|_|"workload_task_failed")?;
                let mut request=active.remove(&completion.id).expect("tracked request");
                recorder.finish(&request,&completion.outcome,completion.started,completion.finished);
                let next_step=request.step+1;
                if next_step<request.steps.len()&&completion.outcome.passed&&!interrupted&&!setup_failed {
                    request.step=next_step;
                    next_task+=1;
                    spawn(&mut request,next_task,&prepared,&client,&mut recorder,&mut tasks,executor);
                    active.insert(next_task,request);
                } else {
                    if next_step<request.steps.len() {
                        recorder.unsent(&request,next_step,if interrupted||setup_failed {"run_cancelled"}else{"dependency_failed"});
                    }
                    if request.sequence.is_some() {
                        if (interrupted||setup_failed)&&next_step<request.steps.len() {sequence_counts.cancelled+=1;}
                        else {sequence_counts.completed+=1;if completion.outcome.passed {sequence_counts.passed+=1;}else{sequence_counts.failed+=1;}}
                    }
                    *group_active.get_mut(&request.group).expect("group reservation")-=1;
                    actor_active.remove(&request.actor);
                }
            },
            _=&mut signal,if !interrupted=>{
                interrupted=true;capture_incomplete=true;stopping=true;
                drain_end=Some(Instant::now()+Duration::from_millis(plan.drain_timeout_ms));
            },
            _=tokio::time::sleep_until(wake)=>{
                if stopping {continue;}
                let Some((index,_))=next else {continue;};
                let cursor=&mut cursors[index];
                let mut request=arrival(&plan,cursor,origin);
                let now=Instant::now();
                let expired=now.saturating_duration_since(request.scheduled)>Duration::from_millis(plan.max_start_lag_ms)
                    ||(cursor.next+1<cursor.total&&origin+Duration::from_nanos(cursor.next_offset_ns())<=now);
                let reason=if expired {Some("scheduler_late")}
                    else if group_active.get(&request.group).copied().unwrap_or(0)>=plan.groups[&request.group].max_in_flight {Some("capacity")}
                    else if actor_active.contains(&request.actor) {Some("actor_busy")}else{None};
                if let Some(reason)=reason {
                    recorder.unsent(&request,0,reason);
                    if request.sequence.is_some(){sequence_counts.unsent+=1;}
                } else {
                    if request.sequence.is_some(){sequence_counts.started+=1;}
                    *group_active.entry(request.group.clone()).or_default()+=1;
                    actor_active.insert(request.actor.clone());
                    next_task+=1;
                    spawn(&mut request,next_task,&prepared,&client,&mut recorder,&mut tasks,executor);
                    active.insert(next_task,request);
                }
                cursor.next+=1;
            }
        }
    }
    let remaining_reason = if setup_failed {
        "setup_failed"
    } else if interrupted {
        "run_cancelled"
    } else {
        "scheduler_late"
    };
    for cursor in &mut cursors {
        if !plan.groups[&cursor.group].sequence.is_empty() {
            sequence_counts.unsent += cursor.total - cursor.next;
        }
        recorder.bulk_unsent(&plan, cursor, remaining_reason);
    }
    let mut aggregate = Metrics::new();
    for metrics in recorder.metrics.values() {
        aggregate.merge(metrics);
    }
    let thresholds: Vec<_> = plan
        .thresholds
        .iter()
        .map(|threshold| evaluate(threshold, &scoped_metrics(&recorder.metrics, threshold)))
        .collect();
    let thresholds_passed = thresholds.iter().all(|row| row["passed"] == true);
    // Only matching error/unsent thresholds can relax the strict default for a
    // series. Every supplied threshold, including overlapping scopes, must pass.
    let failures_allowed = failures_have_allowances(&recorder.metrics, &plan.thresholds);
    let passed = !setup_failed
        && !interrupted
        && !capture_incomplete
        && aggregate.counts.started > 0
        && aggregate.counts.cancelled == 0
        && failures_allowed
        && thresholds_passed;
    let series:Vec<_>=recorder.metrics.iter().map(|((request,group,phase),metrics)|json!({"request":request,"group":group,"phase":phase,"metrics":metrics.json()})).collect();
    let elapsed = origin.elapsed().as_secs_f64();
    let arrival_window_secs = plan
        .phases
        .iter()
        .map(|phase| phase.duration_ms)
        .sum::<u64>() as f64
        / 1000.0;
    Ok(
        json!({"schema_version":2,"kind":"workload","workload_version":1,"mode":"execute","executed":true,"passed":passed,"runner":runner_metadata(),
        "run_id":run_id,"wall_anchor_unix_ms":wall_anchor,"elapsed_secs":elapsed,"interrupted":interrupted,"setup_failed":setup_failed,
        "measurement":{"load_model":"fixed_arrival","clock":"monotonic_offsets_from_wall_anchor","latency_scope":"full_response_and_assertions_or_failure",
            "sequence_child_schedule":"sequence_offer_including_dependency_wait","retries":false,"redirects":false,"environment_proxies":false,
            "group_capacity":"reserved_no_borrowing","late_policy":"skip_expired_no_catch_up","max_start_lag_ms":plan.max_start_lag_ms,
            "error_rate_denominator":"completed","unsent_rate_denominator":"offered","completed_definition":"terminal_attempts_excluding_cancelled",
            "http_response_count_definition":"fully_consumed_response_bodies","max_in_flight":plan.max_in_flight,"timeout_ms":plan.timeout_ms,"drain_timeout_ms":plan.drain_timeout_ms},
        "counts":aggregate.counts.json(),"metrics":aggregate.json(),"series":series,"sequence_counts":sequence_counts.json(),
        "rates":{"arrival_window_secs":arrival_window_secs,"offered_per_sec":aggregate.counts.offered as f64/arrival_window_secs,"started_per_sec":aggregate.counts.started as f64/arrival_window_secs,
            "offered_and_started_denominator":"planned_arrival_window","completed_denominator":"elapsed_including_drain",
            "completed_per_sec":aggregate.counts.completed as f64/elapsed.max(f64::EPSILON)},
        "phases":plan.phases.iter().map(|phase|json!({"name":phase.name,"duration_ms":phase.duration_ms,"rates":phase.rates})).collect::<Vec<_>>(),
        "setup":{"measured":false,"planned":plan.setup.len(),"results":setup_rows},"thresholds":thresholds,
        "evidence":recorder.capture.json(capture_incomplete)}),
    )
}

fn planned_requests(plan: &Plan) -> u64 {
    cursors(plan)
        .iter()
        .map(|cursor| cursor.total * plan.groups[&cursor.group].sequence.len().max(1) as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn plan() -> Plan {
        serde_json::from_value(json!({"workload_version":1,"actors":{"a":{}},"requests":{"read":{"method":"GET","path":"/ok"}},
            "groups":{"load":{"actors":["a"],"max_in_flight":1,"requests":["read"]}},
            "phases":[{"name":"normal","duration_ms":60_000,"rates":{"load":10}}]})).unwrap()
    }

    #[test]
    fn integer_arrivals_are_fixed_and_exclude_window_end() {
        let plan = plan();
        validate(&plan).unwrap();
        let mut cursors = cursors(&plan);
        let cursor = &mut cursors[0];
        assert_eq!(cursor.total, 600);
        assert_eq!(cursor.offset_ns(), 0);
        cursor.next = 599;
        assert_eq!(cursor.offset_ns(), 59_900_000_000);
        assert_eq!(offers(1, 1), 1);
        assert_eq!(offers(1000, 3), 3);
    }

    #[test]
    fn mix_counts_and_sequence_children_are_exact() {
        let mut plan = plan();
        let group = plan.groups.get_mut("load").unwrap();
        group.requests = vec!["read".into(); 3];
        let origin = Instant::now();
        let mut recorder = Recorder {
            origin,
            metrics: BTreeMap::new(),
            capture: Capture::new(&EvidenceOptions::default()),
            next_request: 0,
        };
        initialize_metrics(&plan, &cursors(&plan), &mut recorder);
        assert_eq!(
            recorder
                .metrics
                .values()
                .map(|metrics| metrics.counts.offered)
                .sum::<u64>(),
            600
        );
    }

    #[test]
    fn evidence_stays_bounded_and_reports_every_drop() {
        let mut capture = Capture::new(&EvidenceOptions {
            max_records: 2,
            max_bytes: 1024,
        });
        for _ in 0..5 {
            capture.record(json!({"outcome":"unsent"}));
        }
        let report = capture.json(false);
        assert_eq!(report["captured_records"], 2);
        assert_eq!(report["dropped_records"], 3);
        assert_eq!(report["complete"], false);
        let mut capture = Capture::new(&EvidenceOptions {
            max_records: 100,
            max_bytes: 2,
        });
        capture.record(json!({"outcome":"passed"}));
        assert_eq!(capture.records.len(), 0);
        assert_eq!(capture.dropped, 1);
    }

    #[test]
    fn cancelling_a_million_offers_without_capture_materializes_no_events() {
        let mut plan = plan();
        let request = plan.requests["read"].clone();
        plan.requests.insert("second".into(), request.clone());
        plan.requests.insert("third".into(), request);
        plan.groups.get_mut("load").unwrap().requests =
            vec!["read".into(), "second".into(), "third".into()];
        plan.phases[0].duration_ms = 100_000;
        plan.phases[0].rates.insert("load".into(), 10_000);
        plan.evidence = EvidenceOptions {
            max_records: 0,
            max_bytes: 0,
        };
        validate(&plan).unwrap();
        let origin = Instant::now();
        let mut recorder = Recorder {
            origin,
            metrics: BTreeMap::new(),
            capture: Capture::new(&plan.evidence),
            next_request: 0,
        };
        let mut cursors = cursors(&plan);
        initialize_metrics(&plan, &cursors, &mut recorder);
        assert_eq!(
            recorder.bulk_unsent(&plan, &mut cursors[0], "run_cancelled"),
            0
        );
        assert_eq!(recorder.capture.dropped, 1_000_000);
        assert_eq!(recorder.capture.records.len(), 0);
        assert_eq!(recorder.next_request, 1_000_000);
        assert_eq!(cursors[0].next, 1_000_000);
        let mut aggregate = Metrics::new();
        for metrics in recorder.metrics.values() {
            aggregate.merge(metrics);
        }
        assert_eq!(aggregate.counts.offered, 1_000_000);
        assert_eq!(aggregate.counts.unsent, 1_000_000);
        assert_eq!(aggregate.counts.started, 0);
        for (id, expected) in [("read", 333_334), ("second", 333_333), ("third", 333_333)] {
            let counts = &recorder.metrics[&(id.into(), "load".into(), "normal".into())].counts;
            assert_eq!(counts.unsent, expected);
            assert_eq!(counts.reasons["run_cancelled"], expected);
        }
    }

    #[test]
    fn bulk_cancellation_preserves_partial_cycles_and_only_materializes_retained_prefix() {
        let mut plan = plan();
        plan.requests
            .insert("second".into(), plan.requests["read"].clone());
        plan.groups.get_mut("load").unwrap().requests =
            vec!["read".into(), "second".into(), "read".into()];
        plan.phases[0].duration_ms = 1000;
        let origin = Instant::now();
        let mut recorder = Recorder {
            origin,
            metrics: BTreeMap::new(),
            capture: Capture::new(&EvidenceOptions {
                max_records: 3,
                max_bytes: 4096,
            }),
            next_request: 0,
        };
        let mut cursor = cursors(&plan).remove(0);
        cursor.next = 4;
        assert_eq!(recorder.bulk_unsent(&plan, &mut cursor, "run_cancelled"), 3);
        assert_eq!(recorder.capture.records.len(), 3);
        assert_eq!(recorder.capture.dropped, 3);
        assert_eq!(recorder.next_request, 6);
        assert_eq!(recorder.capture.records[0]["operation"], "second");
        assert_eq!(recorder.capture.records[0]["scheduled_ms"], 400.0);
        assert_eq!(
            recorder.metrics[&("read".into(), "load".into(), "normal".into())]
                .counts
                .unsent,
            4
        );
        assert_eq!(
            recorder.metrics[&("second".into(), "load".into(), "normal".into())]
                .counts
                .unsent,
            2
        );
    }

    #[test]
    fn bulk_sequence_cancellation_counts_every_child_and_repeated_operation() {
        let mut plan = plan();
        let group = plan.groups.get_mut("load").unwrap();
        group.requests.clear();
        group.sequence = vec![
            Step {
                request: "read".into(),
                replay_of: None,
            },
            Step {
                request: "read".into(),
                replay_of: None,
            },
        ];
        let origin = Instant::now();
        let mut recorder = Recorder {
            origin,
            metrics: BTreeMap::new(),
            capture: Capture::new(&EvidenceOptions {
                max_records: 1,
                max_bytes: 4096,
            }),
            next_request: 0,
        };
        let mut cursor = cursors(&plan).remove(0);
        cursor.next = 1;
        assert_eq!(recorder.bulk_unsent(&plan, &mut cursor, "run_cancelled"), 1);
        assert_eq!(
            recorder.metrics[&("read".into(), "load".into(), "normal".into())]
                .counts
                .unsent,
            1198
        );
        assert_eq!(recorder.capture.dropped, 1197);
        assert_eq!(recorder.next_request, 1198);
        assert_eq!(recorder.capture.records[0]["sequence_id"], "normal.load.1");
    }

    #[test]
    fn threshold_equality_passes_and_insufficient_samples_fail() {
        let threshold: Threshold =
            serde_json::from_value(json!({"metric":"error_rate","maximum":0.25,"min_samples":4}))
                .unwrap();
        let mut metrics = Metrics::new();
        metrics.counts.completed = 4;
        metrics.counts.failed = 1;
        assert_eq!(evaluate(&threshold, &metrics)["passed"], true);
        metrics.counts.failed = 2;
        assert_eq!(evaluate(&threshold, &metrics)["passed"], false);
        metrics.counts.completed = 3;
        assert_eq!(
            evaluate(&threshold, &metrics)["reason"],
            "insufficient_samples"
        );
    }

    #[test]
    fn independent_groups_require_reserved_capacity_and_distinct_actors() {
        let mut plan = plan();
        plan.groups.insert(
            "canary".into(),
            Group {
                actors: vec!["a".into()],
                max_in_flight: 1,
                requests: vec!["read".into()],
                sequence: vec![],
            },
        );
        assert_eq!(
            validate(&plan).unwrap_err(),
            "actors_must_belong_to_one_group"
        );
        plan.actors.insert(
            "b".into(),
            Actor {
                headers_env: BTreeMap::new(),
            },
        );
        plan.groups.get_mut("canary").unwrap().actors = vec!["b".into()];
        plan.max_in_flight = 1;
        assert_eq!(
            validate(&plan).unwrap_err(),
            "group_reservations_exceed_global_capacity"
        );
        plan.max_in_flight = 2;
        validate(&plan).unwrap();
    }

    #[test]
    fn replay_requires_same_request_and_runtime_idempotency_key() {
        let mut plan = plan();
        let group = plan.groups.get_mut("load").unwrap();
        group.requests.clear();
        group.sequence = vec![
            Step {
                request: "read".into(),
                replay_of: None,
            },
            Step {
                request: "read".into(),
                replay_of: Some(0),
            },
        ];
        assert!(validate(&plan).is_err());
        plan.requests.get_mut("read").unwrap().idempotency_key_env = Some("TEST_KEY".into());
        validate(&plan).unwrap();
        plan.groups.get_mut("load").unwrap().sequence[1].replay_of = Some(1);
        assert!(validate(&plan).is_err());
    }

    #[test]
    fn lifecycle_counts_reconcile_for_completed_cancelled_and_unsent() {
        let mut counts = Counts {
            offered: 10,
            started: 7,
            completed: 5,
            passed: 4,
            failed: 1,
            cancelled: 2,
            unsent: 3,
            ..Counts::default()
        };
        assert_eq!(counts.offered, counts.started + counts.unsent);
        assert_eq!(counts.started, counts.completed + counts.cancelled);
        assert_eq!(counts.completed, counts.passed + counts.failed);
        assert_eq!(counts.json()["in_flight"], 0);
        counts.merge(&Counts::default());
        assert_eq!(counts.json()["offered"], 10);
    }

    fn options(value: Value) -> Options {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusta-workload-unit-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        Options {
            plan: path,
            execute: true,
            base_url: Some(Url::parse("http://127.0.0.1:1").unwrap()),
            operations: vec!["read".into()],
            allow_writes: false,
        }
    }

    fn fake_http(group: String, _client: Client, _request: Arc<PreparedRequest>) -> ExecuteFuture {
        Box::pin(async move {
            let delay = match group.as_str() {
                "bulk" => 250,
                "canary" => 1,
                "sequence" => 150,
                "slow" => 1000,
                _ => 10,
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Outcome {
                passed: true,
                status: Some(200),
                reason: "passed",
                response_complete: true,
            }
        })
    }

    fn failing_http(
        _group: String,
        _client: Client,
        _request: Arc<PreparedRequest>,
    ) -> ExecuteFuture {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Outcome {
                passed: false,
                status: Some(500),
                reason: "unexpected_status",
                response_complete: true,
            }
        })
    }

    static COUNTED_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

    fn counted_http(
        _group: String,
        _client: Client,
        _request: Arc<PreparedRequest>,
    ) -> ExecuteFuture {
        COUNTED_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Outcome {
                passed: true,
                status: Some(200),
                reason: "passed",
                response_complete: true,
            }
        })
    }

    fn minimal_value() -> Value {
        json!({"workload_version":1,"actors":{"a":{}},"requests":{"read":{"method":"GET","path":"/synthetic"}},
            "groups":{"load":{"actors":["a"],"max_in_flight":1,"requests":["read"]}},
            "phases":[{"name":"normal","duration_ms":1000,"rates":{"load":10}}]})
    }

    #[tokio::test]
    async fn preview_readiness_requires_every_referenced_operation_but_not_unused_specs() {
        let mut value = minimal_value();
        value["requests"]["unused_write"] = json!({"method":"DELETE","path":"/unselected"});
        for selected in [false, true] {
            let mut options = options(value.clone());
            let path = options.plan.clone();
            options.execute = false;
            if !selected {
                options.operations.clear();
            }
            let report = run(options).await.unwrap();
            std::fs::remove_file(path).unwrap();
            assert_eq!(report["executed"], false);
            assert_eq!(report["ready"], selected);
            assert_eq!(report["required_operations"], json!(["read"]));
            assert_eq!(
                report["missing_operations"],
                if selected { json!([]) } else { json!(["read"]) }
            );
            assert_eq!(report["requires_write_opt_in"], false);
            assert_eq!(report["runtime_secrets_loaded"], false);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_keeps_canaries_independent_when_bulk_capacity_is_full() {
        let mut value = minimal_value();
        value["actors"] = json!({"a":{},"b":{}});
        value["max_in_flight"] = json!(2);
        value["groups"] = json!({"bulk":{"actors":["a"],"max_in_flight":1,"requests":["read"]},
            "canary":{"actors":["b"],"max_in_flight":1,"requests":["read"]}});
        value["phases"][0]["rates"] = json!({"bulk":10,"canary":10});
        value["thresholds"] = json!([{"metric":"unsent_rate","group":"bulk","maximum":0.6}]);
        let options = options(value);
        let path = options.plan.clone();
        let report = run_with_executor(options, fake_http).await.unwrap();
        std::fs::remove_file(path).unwrap();
        let bulk = report["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["group"] == "bulk")
            .unwrap();
        let canary = report["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["group"] == "canary")
            .unwrap();
        assert_eq!(bulk["metrics"]["counts"]["offered"], 10);
        assert_eq!(bulk["metrics"]["counts"]["started"], 4);
        assert_eq!(bulk["metrics"]["counts"]["unsent"], 6);
        assert_eq!(canary["metrics"]["counts"]["completed"], 10);
        assert_eq!(canary["metrics"]["counts"]["unsent"], 0);
        assert_eq!(report["counts"]["offered"], 20);
        assert_eq!(report["counts"]["started"], 14);
        assert_eq!(report["counts"]["in_flight"], 0);
        assert_eq!(report["passed"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn scoped_failure_allowance_does_not_cover_another_series() {
        for include_uncovered in [false, true] {
            let mut value = minimal_value();
            value["phases"][0]["duration_ms"] = json!(100);
            value["phases"][0]["rates"] = json!({"load":1});
            value["thresholds"] = json!([{"metric":"error_rate","group":"load","maximum":1.0}]);
            if include_uncovered {
                value["actors"]["b"] = json!({});
                value["groups"]["other"] =
                    json!({"actors":["b"],"max_in_flight":1,"requests":["read"]});
                value["phases"][0]["rates"]["other"] = json!(1);
            }
            let options = options(value);
            let path = options.plan.clone();
            let report = run_with_executor(options, failing_http).await.unwrap();
            std::fs::remove_file(path).unwrap();
            assert_eq!(report["thresholds"][0]["passed"], true);
            assert_eq!(report["passed"], !include_uncovered);
            assert_eq!(
                report["counts"]["failed"],
                if include_uncovered { 2 } else { 1 }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_global_and_scoped_thresholds_must_all_pass() {
        for global_maximum in [0.5, 1.0] {
            let mut value = minimal_value();
            value["phases"][0]["duration_ms"] = json!(100);
            value["phases"][0]["rates"] = json!({"load":1});
            value["thresholds"] = json!([
                {"metric":"error_rate","group":"load","maximum":1.0},
                {"metric":"error_rate","maximum":global_maximum}
            ]);
            let options = options(value);
            let path = options.plan.clone();
            let report = run_with_executor(options, failing_http).await.unwrap();
            std::fs::remove_file(path).unwrap();
            assert_eq!(report["thresholds"][0]["passed"], true);
            assert_eq!(report["thresholds"][1]["passed"], global_maximum == 1.0);
            assert_eq!(report["passed"], global_maximum == 1.0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn acceptance_schedules_600_requests_over_60_seconds_across_three_routes() {
        COUNTED_ATTEMPTS.store(0, Ordering::Relaxed);
        let mut value = minimal_value();
        value["requests"]["second"] = json!({"method":"GET","path":"/second"});
        value["requests"]["third"] = json!({"method":"GET","path":"/third"});
        value["groups"]["load"]["requests"] = json!(["read", "second", "third"]);
        value["phases"][0]["duration_ms"] = json!(60_000);
        value["phases"][0]["rates"] = json!({"load":10});
        let mut options = options(value);
        let path = options.plan.clone();
        options.operations = vec!["read".into(), "second".into(), "third".into()];
        let report = run_with_executor(options, counted_http).await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(report["passed"], true);
        assert_eq!(COUNTED_ATTEMPTS.load(Ordering::Relaxed), 600);
        for field in ["offered", "started", "completed", "passed"] {
            assert_eq!(report["counts"][field], 600);
        }
        for field in ["failed", "unsent", "cancelled", "in_flight"] {
            assert_eq!(report["counts"][field], 0);
        }
        assert_eq!(report["series"].as_array().unwrap().len(), 3);
        for series in report["series"].as_array().unwrap() {
            for field in ["offered", "started", "completed"] {
                assert_eq!(series["metrics"]["counts"][field], 200);
            }
        }
        let records = report["evidence"]["records"].as_array().unwrap();
        assert_eq!(records.len(), 600);
        for (index, record) in records.iter().enumerate() {
            let scheduled = record["scheduled_ms"].as_f64().unwrap();
            assert!((scheduled - index as f64 * 100.0).abs() < 0.000_001);
            let started = record["started_ms"].as_f64().unwrap();
            assert!((0.0..=20.0).contains(&(started - scheduled)));
            assert_eq!(record["outcome"], "passed");
        }
        assert!((report["elapsed_secs"].as_f64().unwrap() - 60.0).abs() < 0.001);
        assert_eq!(report["rates"]["offered_per_sec"], 10.0);
        assert_eq!(report["evidence"]["complete"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn admitted_sequence_finishes_during_drain_after_arrivals_close() {
        let mut value = minimal_value();
        value["groups"] = json!({"sequence":{"actors":["a"],"max_in_flight":1,
            "sequence":[{"request":"read"},{"request":"read"}]}});
        value["phases"][0]["duration_ms"] = json!(100);
        value["phases"][0]["rates"] = json!({"sequence":1});
        let options = options(value);
        let path = options.plan.clone();
        let report = run_with_executor(options, fake_http).await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(report["passed"], true);
        assert_eq!(report["counts"]["offered"], 2);
        assert_eq!(report["counts"]["completed"], 2);
        assert_eq!(report["counts"]["unsent"], 0);
        assert_eq!(report["sequence_counts"]["completed"], 1);
        assert!(report["elapsed_secs"].as_f64().unwrap() >= 0.300);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_slots_are_unsent_without_a_catch_up_burst() {
        let options = options(minimal_value());
        let path = options.plan.clone();
        let handle = tokio::spawn(run_with_executor(options, fake_http));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(350)).await;
        let report = handle.await.unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(report["counts"]["unsent"].as_u64().unwrap() >= 3);
        assert!(
            report["counts"]["reasons"]["scheduler_late"]
                .as_u64()
                .unwrap()
                >= 3
        );
        for record in report["evidence"]["records"].as_array().unwrap() {
            if let Some(started) = record["started_ms"].as_f64() {
                assert!(!(100.0..400.0).contains(&started));
            }
        }
        assert_eq!(report["counts"]["in_flight"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_timeout_produces_cancelled_accounting_and_incomplete_evidence() {
        let mut value = minimal_value();
        value["groups"] = json!({"slow":{"actors":["a"],"max_in_flight":1,"requests":["read"]}});
        value["phases"][0]["duration_ms"] = json!(100);
        value["phases"][0]["rates"] = json!({"slow":1});
        value["drain_timeout_ms"] = json!(50);
        let options = options(value);
        let path = options.plan.clone();
        let report = run_with_executor(options, fake_http).await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(report["passed"], false);
        assert_eq!(report["counts"]["offered"], 1);
        assert_eq!(report["counts"]["started"], 1);
        assert_eq!(report["counts"]["cancelled"], 1);
        assert_eq!(report["counts"]["completed"], 0);
        assert_eq!(report["counts"]["in_flight"], 0);
        assert_eq!(report["evidence"]["complete"], false);
        assert!(
            report["evidence"]["records"][0]["started_ms"]
                .as_f64()
                .is_some()
        );
    }
}
