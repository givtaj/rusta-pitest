use clap::{Args, Parser, Subcommand};
use reqwest::Url;

use crate::catalogue::{ROOTS_BASE_URL, Target, parse_base_url, parse_target};

#[derive(Parser)]
#[command(
    version,
    about = "Send GET or POST requests and measure response times"
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
