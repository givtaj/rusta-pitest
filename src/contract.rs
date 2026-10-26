use reqwest::{StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub const BODY_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub struct SimulationContract {
    method: &'static str,
    blocks_thread: bool,
    delay_ms: u32,
    status_code: u16,
    scenario: Option<&'static str>,
}
impl SimulationContract {
    pub fn for_route(
        route: &str,
        delay_ms: u32,
        status_code: u16,
        scenario: Option<&'static str>,
    ) -> Option<Self> {
        let (method, blocks_thread) = match route {
            "/sleep/tokio" => ("tokio_sleep", false),
            "/sleep/tokio-spawn" => ("tokio_spawn", false),
            "/sleep/thread" => ("thread_sleep", true),
            "/sleep/park" => ("thread_park", true),
            _ => return None,
        };
        Some(Self {
            method,
            blocks_thread,
            delay_ms,
            status_code,
            scenario,
        })
    }
    pub fn validate(&self, status: StatusCode, captured: &ContractBody) -> Vec<&'static str> {
        if captured.too_large {
            return vec!["body_too_large"];
        }
        let body = match serde_json::from_slice::<Value>(&captured.bytes) {
            Ok(body) if body.is_object() => body,
            _ => return vec!["invalid_json"],
        };
        if body.get("simulated") == Some(&json!(false))
            && body.pointer("/error/code").and_then(Value::as_str) == Some("capacity_exceeded")
        {
            return vec!["capacity_exceeded"];
        }
        let mut failures = Vec::new();
        if status.as_u16() != self.status_code {
            failures.push("http_status");
        }
        for (field, expected) in [
            ("operation", json!("sleep")),
            ("simulated", json!(true)),
            ("method", json!(self.method)),
            ("blocks_thread", json!(self.blocks_thread)),
            ("requested_duration_ms", json!(self.delay_ms)),
            ("status_code", json!(self.status_code)),
        ] {
            if body.get(field) != Some(&expected) {
                failures.push(field);
            }
        }
        let scenario_matches = match self.scenario {
            Some(scenario) => body.get("scenario").and_then(Value::as_str) == Some(scenario),
            None => !body.as_object().unwrap().contains_key("scenario"),
        };
        if !scenario_matches {
            failures.push("scenario");
        }
        if self.status_code >= 400 {
            if let Some(error) = body.get("error").and_then(Value::as_object) {
                if error.get("code").and_then(Value::as_str) != Some(error_code(self.status_code)) {
                    failures.push("error.code");
                }
                let retryable = matches!(self.status_code, 408 | 429 | 500 | 502 | 503 | 504);
                if error.get("retryable").and_then(Value::as_bool) != Some(retryable) {
                    failures.push("error.retryable");
                }
            } else {
                failures.push("error");
            }
        } else if body.get("error").is_some() {
            failures.push("error");
        }
        let retry_after_matches = if matches!(self.status_code, 429 | 503) {
            captured.retry_after_count == 1 && captured.retry_after_one
        } else {
            captured.retry_after_count == 0
        };
        if !retry_after_matches {
            failures.push("retry_after");
        }
        failures
    }
}
fn error_code(status: u16) -> &'static str {
    match status {
        400 => "invalid_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        408 => "request_timeout",
        409 => "conflict",
        413 => "payload_too_large",
        415 => "unsupported_media_type",
        422 => "validation_failed",
        429 => "rate_limited",
        500 => "internal_error",
        502 => "bad_gateway",
        503 => "service_unavailable",
        504 => "upstream_timeout",
        _ if (400..500).contains(&status) => "client_error",
        _ => "server_error",
    }
}
pub struct ContractBody {
    bytes: Vec<u8>,
    too_large: bool,
    retry_after_count: usize,
    retry_after_one: bool,
}
impl ContractBody {
    pub fn new(headers: &HeaderMap) -> Self {
        Self {
            bytes: Vec::new(),
            too_large: false,
            retry_after_count: headers.get_all(reqwest::header::RETRY_AFTER).iter().count(),
            retry_after_one: headers
                .get(reqwest::header::RETRY_AFTER)
                .is_some_and(|value| value == "1"),
        }
    }
    pub fn capture(&mut self, chunk: &[u8]) {
        if self.too_large {
            return;
        }
        if chunk.len() > BODY_LIMIT_BYTES - self.bytes.len() {
            self.too_large = true;
            self.bytes.clear();
        } else {
            // Avoid Vec's doubling policy retaining more than the body limit.
            self.bytes.reserve_exact(chunk.len());
            self.bytes.extend_from_slice(chunk);
        }
    }
}
#[derive(Default)]
pub struct ContractStats {
    pub checked: u64,
    pub failed: u64,
    failures: BTreeMap<&'static str, u64>,
}
impl ContractStats {
    pub fn record(&mut self, reasons: Vec<&'static str>) {
        self.checked += 1;
        self.failed += u64::from(!reasons.is_empty());
        for reason in reasons {
            *self.failures.entry(reason).or_default() += 1;
        }
    }
    pub fn merge(&mut self, other: Self) {
        self.checked += other.checked;
        self.failed += other.failed;
        for (reason, count) in other.failures {
            *self.failures.entry(reason).or_default() += count;
        }
    }
    pub fn to_json(&self) -> Value {
        json!({ "scope": "all_completed_responses", "body_limit_bytes": BODY_LIMIT_BYTES,
            "checked": self.checked, "passed": self.checked - self.failed,
            "failed": self.failed, "failures": self.failures })
    }
    pub fn print(&self) {
        println!("Contract checks: {}", self.checked);
        println!("Contract failures: {}", self.failed);
        if !self.failures.is_empty() {
            println!(
                "Contract failure reasons: {}",
                self.failures
                    .iter()
                    .map(|(reason, count)| format!("{reason}={count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}
