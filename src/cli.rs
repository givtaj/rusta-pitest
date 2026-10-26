use clap::{Args, Parser, Subcommand};
use reqwest::Url;

use crate::catalogue::{ROOTS_BASE_URL, Target, parse_base_url, parse_target};

#[derive(Parser)]
#[command(
    version,
    about = "Test HTTP APIs, generate OpenAPI suites, and measure response times"
)]
pub struct Cli {
    /// Save a versioned JSON report to a new file (keeps terminal output)
    #[arg(long, global = true, value_name = "FILE")]
    pub output: Option<std::path::PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Generate a reviewable suite from a raw OpenAPI contract
    Openapi {
        #[command(subcommand)]
        command: OpenApiCommand,
    },
    /// Preview or explicitly execute a generated contract suite
    Suite {
        #[command(subcommand)]
        command: SuiteCommand,
    },
    /// Open the guided request wizard (also used when no command is given)
    Wizard,
    /// List available targets without sending requests
    #[command(alias = "catalog")]
    Catalogue,
    /// Benchmark all four SleepAPI waiting endpoints with the same workload
    Profile(crate::profile::ProfileOptions),
    /// Run a named catalogue target with its expected status
    Target {
        #[arg(value_parser = parse_target)]
        target: Target,
        /// API origin; no path, credentials, query or fragment
        #[arg(long, default_value = ROOTS_BASE_URL, value_parser = parse_base_url)]
        base_url: Url,
        #[command(flatten)]
        run: RunOptions,
    },
    /// Send GET requests
    Get(Options),
    /// Send POST requests, optionally with a JSON body
    Post {
        #[command(flatten)]
        options: Options,
        /// JSON body (sent as application/json)
        #[arg(long, value_parser = parse_json)]
        json: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum OpenApiCommand {
    /// Read OpenAPI 3.1 JSON/YAML from a local path or HTTP(S) URL
    Generate {
        source: String,
        /// Write a new suite file; existing files are never overwritten
        #[arg(long, value_name = "FILE")]
        out: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
pub enum SuiteCommand {
    /// Preview by default; --execute sends selected GET/HEAD requests
    Run(SuiteOptions),
}

#[derive(Args)]
pub struct SuiteOptions {
    pub suite: std::path::PathBuf,
    /// Explicit API base URL, including an optional path prefix
    #[arg(long, value_parser = parse_url)]
    pub base_url: Option<Url>,
    /// Send requests after all selected cases pass preflight
    #[arg(long)]
    pub execute: bool,
    /// Select an operation ID; repeat for multiple operations
    #[arg(long = "operation")]
    pub operations: Vec<String>,
    /// Bind a fixture slot: NAME=VALUE; values are not saved in reports
    #[arg(long = "fixture", value_parser = parse_assignment)]
    pub fixtures: Vec<(String, String)>,
    /// Bind a security scheme to an environment variable: SCHEME=ENV_NAME
    #[arg(long = "auth-env", value_parser = parse_assignment)]
    pub auth_env: Vec<(String, String)>,
    /// Timeout through the full response body
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u32).range(1..))]
    pub timeout_ms: u32,
    /// Maximum request starts per second; requests execute sequentially
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=100))]
    pub rate_per_second: u16,
    /// Refuse selections above this request budget
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub max_requests: u32,
}

fn parse_assignment(input: &str) -> Result<(String, String), String> {
    let (name, value) = input.split_once('=').ok_or("expected NAME=VALUE")?;
    if name.is_empty() || value.is_empty() {
        return Err("expected a non-empty NAME and VALUE".into());
    }
    Ok((name.to_owned(), value.to_owned()))
}

#[derive(Args)]
pub struct Options {
    /// Absolute HTTP or HTTPS URL
    #[arg(value_parser = parse_url)]
    pub url: Url,
    #[command(flatten)]
    pub run: RunOptions,
}

#[derive(Args)]
pub struct RunOptions {
    /// Total requests to send; defaults to one when no duration is given
    #[arg(short = 'n', long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub requests: u32,
    /// Send requests for this many seconds, then finish in-flight requests
    #[arg(long, conflicts_with = "requests", value_parser = clap::value_parser!(u32).range(1..))]
    pub duration_secs: Option<u32>,
    /// Maximum concurrent requests
    #[arg(short = 'c', long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,
    /// Per-request timeout, including the full response body
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u32).range(1..))]
    pub timeout_ms: u32,
    /// Required status; defaults to the target's status, or any 2xx for get/post
    #[arg(long, value_parser = parse_status)]
    pub expect_status: Option<u16>,
}

pub(crate) fn parse_url(input: &str) -> Result<Url, String> {
    let url = Url::parse(input).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("expected an absolute http:// or https:// URL".into());
    }
    Ok(url)
}

pub(crate) fn parse_json(input: &str) -> Result<String, String> {
    serde_json::from_str::<serde_json::Value>(input)
        .map_err(|error| format!("invalid JSON: {error}"))?;
    Ok(input.to_owned())
}

pub(crate) fn parse_status(input: &str) -> Result<u16, String> {
    input
        .parse::<u16>()
        .ok()
        .filter(|status| (100..600).contains(status))
        .ok_or_else(|| "expected an HTTP status from 100 to 599".into())
}
