use std::io::{self, Write};

use reqwest::Method;

use crate::{
    catalogue::{ROOTS_BASE_URL, ROOTS_TARGETS, parse_base_url, parse_target},
    cli::{Options, RunOptions, parse_json, parse_status, parse_url},
};

pub enum Request {
    Http(Method, Options, Option<String>),
    Profile(crate::profile::ProfileOptions),
}

enum PromptError {
    Cancelled,
    Io(io::Error),
}

impl From<io::Error> for PromptError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn run(out: &mut impl Write) -> io::Result<Option<Request>> {
    match configure(out) {
        Ok(request) => Ok(Some(request)),
        Err(PromptError::Cancelled) => {
            writeln!(out, "Cancelled.")?;
            Ok(None)
        }
        Err(PromptError::Io(error)) => Err(error),
    }
}

fn configure(out: &mut impl Write) -> Result<Request, PromptError> {
    writeln!(out, "rusta-pitest")?;
    writeln!(
        out,
        "Press Enter to accept a default, or type q to quit at any prompt.\n"
    )?;
    writeln!(out, "Choose a request:")?;
    writeln!(out, "  1) Roots greeting — GET /api/hello")?;
    writeln!(out, "  2) Roots database health — GET /api/health")?;
    writeln!(out, "  3) Custom GET")?;
    writeln!(out, "  4) Custom POST")?;
    writeln!(out, "  5) Roots catalogue")?;
    writeln!(
        out,
        "  6) SleepAPI benchmark profile — all four waiting endpoints"
    )?;
    let target = ask(out, "Request", Some("1"), |input| match input {
        "1" | "2" | "3" | "4" | "5" | "6" => Ok(input.to_owned()),
        _ => Err("Choose 1, 2, 3, 4, 5, or 6.".into()),
    })?;

    if target == "6" {
        return configure_sleepapi(out);
    }

    let preset = match target.as_str() {
        "1" => Some(ROOTS_TARGETS[0]),
        "2" => Some(ROOTS_TARGETS[1]),
        "5" => {
            writeln!(out, "\nRoots catalogue:")?;
            for (index, preset) in ROOTS_TARGETS.iter().enumerate() {
                writeln!(
                    out,
                    "  {}) {} — GET {} — expect {}",
                    index + 1,
                    preset.label,
                    preset.path,
                    preset.expected_status
                )?;
            }
            Some(ask(out, "Target (number or ID)", Some("1"), |input| {
                input
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .and_then(|index| ROOTS_TARGETS.get(index).copied())
                    .map(Ok)
                    .unwrap_or_else(|| parse_target(input))
            })?)
        }
        _ => None,
    };
    let (method, url) = match target.as_str() {
        "1" | "2" | "5" => {
            let preset = preset.unwrap();
            writeln!(out, "\n{}", preset.description)?;
            let base = if target == "5" {
                ask(
                    out,
                    "Roots API base URL",
                    Some(ROOTS_BASE_URL),
                    parse_base_url,
                )?
            } else {
                parse_base_url(ROOTS_BASE_URL).unwrap()
            };
            (Method::GET, preset.url(&base))
        }
        _ => {
            let method = if target == "4" {
                Method::POST
            } else {
                Method::GET
            };
            (method, ask(out, "URL", None, parse_url)?)
        }
    };
    let json = if method == Method::POST {
        ask(
            out,
            "JSON body (one line; Enter for an empty body)",
            Some(""),
            |input| {
                if input.is_empty() {
                    Ok(None)
                } else {
                    parse_json(input).map(Some)
                }
            },
        )?
    } else {
        None
    };

    writeln!(out, "\nChoose a run:")?;
    writeln!(out, "  1) Send one request")?;
    writeln!(out, "  2) Benchmark")?;
    writeln!(out, "  3) Benchmark for a duration")?;
    let run_type = ask(out, "Run type", Some("1"), |input| match input {
        "1" => Ok(1),
        "2" => Ok(2),
        "3" => Ok(3),
        _ => Err("Choose 1, 2, or 3.".into()),
    })?;
    let (requests, duration_secs, concurrency) = match run_type {
        2 => {
            let requests = ask(out, "Total requests", Some("100"), |input| {
                positive(input, u32::MAX)
            })?;
            let default_concurrency = requests.min(4).to_string();
            let concurrency = ask(
                out,
                "Concurrent requests",
                Some(&default_concurrency),
                |input| positive(input, u32::from(u16::MAX)).map(|value| value as u16),
            )?;
            (requests, None, concurrency)
        }
        3 => {
            let duration_secs = ask(out, "Duration (seconds)", Some("60"), |input| {
                positive(input, u32::MAX)
            })?;
            let concurrency = ask(out, "Concurrent requests", Some("4"), |input| {
                positive(input, u32::from(u16::MAX)).map(|value| value as u16)
            })?;
            (1, Some(duration_secs), concurrency)
        }
        _ => (1, None, 1),
    };
    let timeout_ms = ask(out, "Timeout per request (ms)", Some("5000"), |input| {
        positive(input, u32::MAX)
    })?;
    let default_status = preset.map_or_else(|| "2xx".into(), |p| p.expected_status.to_string());
    let expect_status = ask(
        out,
        "Expected status (2xx for any success)",
        Some(&default_status),
        |input| {
            if input.eq_ignore_ascii_case("2xx") {
                Ok(None)
            } else {
                parse_status(input).map(Some)
            }
        },
    )?;

    let options = Options {
        url,
        run: RunOptions {
            requests,
            duration_secs,
            concurrency,
            timeout_ms,
            expect_status,
        },
    };
    writeln!(out, "\nReady to run:")?;
    writeln!(out, "  {method} {}", options.url)?;
    if let Some(duration_secs) = duration_secs {
        writeln!(out, "  Duration: {duration_secs} seconds")?;
        writeln!(out, "  Concurrency: {concurrency}")?;
    } else {
        writeln!(out, "  Requests: {requests}")?;
        writeln!(
            out,
            "  Concurrency: {}",
            u32::from(concurrency).min(requests)
        )?;
    }
    writeln!(out, "  Timeout: {timeout_ms} ms")?;
    writeln!(
        out,
        "  Expected status: {}",
        expect_status.map_or_else(|| "2xx".into(), |status| status.to_string())
    )?;
    if let Some(body) = &json {
        writeln!(out, "  JSON body: {body}")?;
    } else if method == Method::POST {
        writeln!(out, "  Body: empty")?;
    }
    let confirmed = ask(out, "Run? (y/n)", Some("y"), |input| {
        if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
            Ok(true)
        } else if input.eq_ignore_ascii_case("n") || input.eq_ignore_ascii_case("no") {
            Ok(false)
        } else {
            Err("Enter y to run or n to cancel.".into())
        }
    })?;
    if !confirmed {
        return Err(PromptError::Cancelled);
    }

    writeln!(out, "\nRunning…\n")?;
    Ok(Request::Http(method, options, json))
}

fn configure_sleepapi(out: &mut impl Write) -> Result<Request, PromptError> {
    use crate::profile::{
        ProfileName, ProfileOptions, SLEEPAPI_BASE_URL, SLEEPAPI_ROUTES, Scenario,
        parse_response_status,
    };
    use clap::ValueEnum;

    writeln!(
        out,
        "\nSleepAPI: run the same workload against four endpoints in sequence."
    )?;
    let base_url = ask(
        out,
        "SleepAPI base URL",
        Some(SLEEPAPI_BASE_URL),
        parse_base_url,
    )?;
    writeln!(
        out,
        "  baseline — configurable delay and response status (default: 400 ms, 200)"
    )?;
    for scenario in Scenario::value_variants() {
        let (delay, status) = scenario.defaults();
        writeln!(out, "  {} — {delay} ms, HTTP {status}", scenario.name())?;
    }
    let scenario = ask(out, "Scenario", Some("baseline"), |input| {
        if input == "baseline" {
            Ok(None)
        } else {
            Scenario::from_str(input, false).map(Some)
        }
    })?;
    let (default_delay, default_status) = scenario.map_or((400, 200), Scenario::defaults);
    let delay_ms = ask(
        out,
        "Server delay (ms)",
        Some(&default_delay.to_string()),
        |input| {
            input
                .parse::<u32>()
                .ok()
                .filter(|value| *value <= 3_600_000)
                .ok_or_else(|| "Enter a whole number from 0 to 3600000.".into())
        },
    )?;
    let status_code = if scenario.is_none() {
        Some(ask(
            out,
            "Server response status",
            Some("200"),
            parse_response_status,
        )?)
    } else {
        None
    };
    let expected_default = status_code.unwrap_or(default_status);

    writeln!(
        out,
        "\nChoose a run (settings apply to each of the four endpoints):"
    )?;
    writeln!(out, "  1) One request per endpoint")?;
    writeln!(out, "  2) Benchmark by request count")?;
    writeln!(out, "  3) Benchmark for a duration")?;
    let run_type = ask(out, "Run type", Some("2"), |input| match input {
        "1" => Ok(1),
        "2" => Ok(2),
        "3" => Ok(3),
        _ => Err("Choose 1, 2, or 3.".into()),
    })?;
    let (requests, duration_secs, concurrency) = match run_type {
        2 => {
            let requests = ask(out, "Requests per endpoint", Some("100"), |input| {
                positive(input, u32::MAX)
            })?;
            let concurrency = ask(
                out,
                "Concurrent requests",
                Some(&requests.min(4).to_string()),
                |input| positive(input, u32::from(u16::MAX)).map(|value| value as u16),
            )?;
            (requests, None, concurrency)
        }
        3 => {
            let seconds = ask(
                out,
                "Duration per endpoint (seconds)",
                Some("30"),
                |input| positive(input, u32::MAX),
            )?;
            let concurrency = ask(out, "Concurrent requests", Some("4"), |input| {
                positive(input, u32::from(u16::MAX)).map(|value| value as u16)
            })?;
            (1, Some(seconds), concurrency)
        }
        _ => (1, None, 1),
    };
    if matches!(scenario, Some(Scenario::ClientTimeout)) {
        writeln!(
            out,
            "Choose a timeout shorter than {delay_ms} ms to cause client timeouts; they count as failures."
        )?;
    }
    let timeout_ms = ask(out, "Timeout per request (ms)", Some("5000"), |input| {
        positive(input, u32::MAX)
    })?;
    let expect_status = ask(
        out,
        "Expected status",
        Some(&expected_default.to_string()),
        parse_status,
    )?;

    writeln!(out, "\nReady to run SleepAPI profile:")?;
    for route in SLEEPAPI_ROUTES {
        writeln!(
            out,
            "  POST {}",
            base_url.join(route).expect("valid SleepAPI route")
        )?;
    }
    let mut body = if let Some(scenario) = scenario {
        serde_json::json!({"scenario": scenario.name()})
    } else {
        serde_json::json!({"status_code": status_code.unwrap()})
    };
    body["duration_ms"] = serde_json::json!(delay_ms);
    writeln!(out, "  JSON body: {body}")?;
    if let Some(seconds) = duration_secs {
        writeln!(
            out,
            "  Duration: {seconds} seconds per endpoint (four sequential windows, plus in-flight completion)"
        )?;
    } else {
        writeln!(
            out,
            "  Requests: {requests} per endpoint ({} total)",
            u64::from(requests) * 4
        )?;
    }
    let effective_concurrency = if duration_secs.is_some() {
        u32::from(concurrency)
    } else {
        u32::from(concurrency).min(requests)
    };
    writeln!(out, "  Concurrency: {effective_concurrency}")?;
    writeln!(out, "  Timeout: {timeout_ms} ms")?;
    writeln!(out, "  Expected status: {expect_status}")?;
    writeln!(
        out,
        "  Status checks only; no automatic retries or error-body assertions."
    )?;
    let confirmed = ask(out, "Run? (y/n)", Some("y"), |input| {
        match input.to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            _ => Err("Enter y to run or n to cancel.".into()),
        }
    })?;
    if !confirmed {
        return Err(PromptError::Cancelled);
    }
    Ok(Request::Profile(ProfileOptions {
        profile: ProfileName::Sleepapi,
        base_url,
        requests,
        duration_secs,
        concurrency,
        timeout_ms,
        expect_status: Some(expect_status),
        scenario,
        delay_ms: Some(delay_ms),
        status_code,
        dry_run: false,
        validate_contract: false,
    }))
}

fn positive(input: &str, max: u32) -> Result<u32, String> {
    input
        .parse::<u32>()
        .ok()
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| format!("Enter a whole number from 1 to {max}."))
}

fn ask<T>(
    out: &mut impl Write,
    label: &str,
    default: Option<&str>,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, PromptError> {
    loop {
        write!(out, "{label}")?;
        if let Some(default) = default.filter(|value| !value.is_empty()) {
            write!(out, " [{default}]")?;
        }
        write!(out, ": ")?;
        out.flush()?;

        let mut line = String::new();
        // EOF, including an incomplete final line, must cancel before defaults
        // or explicit answers can accidentally start requests. LF and CRLF count
        // as completed lines; a bare CR does not.
        if io::stdin().read_line(&mut line)? == 0 || !line.ends_with('\n') {
            return Err(PromptError::Cancelled);
        }
        let input = line.trim();
        if input.eq_ignore_ascii_case("q") {
            return Err(PromptError::Cancelled);
        }
        let input = if input.is_empty() {
            default.unwrap_or("")
        } else {
            input
        };
        match parse(input) {
            Ok(value) => return Ok(value),
            Err(message) => writeln!(out, "  {message}")?,
        }
    }
}
