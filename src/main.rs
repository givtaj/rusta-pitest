mod bench;
mod catalogue;
mod cli;
mod contract;
mod cookie;
mod model;
mod openapi;
mod output;
mod profile;
mod schema;
mod strict_json;
mod suite_run;
mod wizard;
mod workload;
mod workload_http;

use std::{
    io::{self, Write},
    path::Path,
    process::ExitCode,
};

use clap::{CommandFactory, Parser, error::ErrorKind};
use reqwest::Method;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut terminal = output::Terminal::new(cli.output.is_some());
    let result = run(cli, &mut terminal).await;
    match result.and_then(|code| terminal.finish().map(|()| code)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Cannot write stdout: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli, out: &mut impl Write) -> io::Result<ExitCode> {
    if cli.output.is_some()
        && (matches!(&cli.command, Some(Command::Catalogue))
            || matches!(&cli.command, Some(Command::Openapi { .. }))
            || matches!(&cli.command, Some(Command::Profile(options)) if options.dry_run))
    {
        Cli::command()
            .error(
                ErrorKind::ArgumentConflict,
                "--output requires a benchmark or suite run; use --out with openapi generate",
            )
            .exit();
    }
    let destination = cli.output.as_deref();
    let (method, options, json) = match cli.command {
        Some(Command::Openapi {
            command:
                cli::OpenApiCommand::Generate {
                    source,
                    out,
                    max_input_bytes,
                    max_suite_bytes,
                },
        }) => {
            return Ok(generate_suite(&source, &out, max_input_bytes, max_suite_bytes).await);
        }
        Some(Command::Suite {
            command: cli::SuiteCommand::Run(options),
        }) => {
            return run_suite(options, destination, out).await;
        }
        Some(Command::Workload {
            command: cli::WorkloadCommand::Run(options),
        }) => {
            return run_workload(options, destination, out).await;
        }
        Some(Command::Catalogue) => {
            catalogue::print();
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Profile(options)) => return Ok(run_profile(options, destination, out).await),
        Some(Command::Target {
            target,
            base_url,
            mut run,
        }) => {
            let expected_status = *run.expect_status.get_or_insert(target.expected_status);
            let url = target.url(&base_url);
            writeln!(out, "Target: {} — {}", target.id, target.label)?;
            writeln!(out, "{}", target.description)?;
            writeln!(out, "GET {url}")?;
            writeln!(out, "Expected status: {expected_status}\n")?;
            (Method::GET, cli::Options { url, run }, None)
        }
        Some(Command::Get(options)) => (Method::GET, options, None),
        Some(Command::Post { options, json }) => (Method::POST, options, json),
        Some(Command::Wizard) | None => match wizard::run(out) {
            Ok(Some(wizard::Request::Http(method, options, json))) => (method, options, json),
            Ok(Some(wizard::Request::Profile(options))) => {
                return Ok(run_profile(options, destination, out).await);
            }
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Err(error) => {
                eprintln!("Wizard error: {error}");
                return Ok(ExitCode::FAILURE);
            }
        },
    };

    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return Ok(code),
    };
    let report = match bench::run(method, options, json).await {
        Ok(report) => {
            report.print(out)?;
            Some(report)
        }
        Err(error) => {
            eprintln!("Error: {error}");
            None
        }
    };
    Ok(finish(
        output::Document {
            kind: "request",
            profile: None,
            runs: vec![output::RunRecord {
                route: None,
                report,
            }],
        },
        file,
    ))
}

async fn generate_suite(
    source: &str,
    out: &Path,
    max_input_bytes: u64,
    max_suite_bytes: u64,
) -> ExitCode {
    let file = match output::ReportFile::prepare(out) {
        Ok(file) => file,
        Err(_) => {
            eprintln!("Cannot prepare suite output; choose a new writable file path");
            return ExitCode::FAILURE;
        }
    };
    match openapi::generate(source, max_input_bytes).await {
        Ok(suite) => {
            let value = serde_json::to_value(&suite).expect("suite is serializable");
            let bytes = serde_json::to_vec_pretty(&value).expect("suite is serializable");
            // finish_value appends a final newline; it counts toward the output limit.
            if bytes.len() as u64 >= max_suite_bytes
                || strict_json::parse_json_with_limit(&bytes, max_suite_bytes as usize).is_err()
            {
                eprintln!("Generated suite exceeds the supported size or complexity limits");
                return ExitCode::FAILURE;
            }
            if file.finish_value(&value).is_err() {
                eprintln!("Cannot publish suite output");
                return ExitCode::FAILURE;
            }
            println!(
                "Generated {} cases. Review the suite, then use suite run to preview it.",
                suite.cases.len()
            );
            for case in &suite.cases {
                println!(
                    "{} {} — {}{}",
                    case.method,
                    case.path,
                    case.id,
                    if case.blockers.is_empty() {
                        ""
                    } else {
                        " [blocked]"
                    }
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("OpenAPI generation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_suite(
    options: cli::SuiteOptions,
    destination: Option<&Path>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return Ok(code),
    };
    let result = suite_run::run(suite_run::RunOptions {
        suite: options.suite,
        max_input_bytes: options.max_input_bytes,
        base_url: options.base_url,
        execute: options.execute,
        operations: options.operations,
        fixtures: options.fixtures,
        auth_env: options.auth_env,
        timeout_ms: options.timeout_ms,
        rate_per_second: options.rate_per_second,
        max_requests: options.max_requests,
    })
    .await;
    match result {
        Ok(report) => finish_json_report(&report, file, out, options.execute),
        Err(error) => {
            eprintln!("Suite preflight failed: {error}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn prepare_report(destination: Option<&Path>) -> Result<Option<output::ReportFile>, ExitCode> {
    destination
        .map(output::ReportFile::prepare)
        .transpose()
        .map_err(|error| {
            eprintln!("Cannot prepare JSON report: {error}");
            ExitCode::FAILURE
        })
}

async fn run_workload(
    options: cli::WorkloadOptions,
    destination: Option<&Path>,
    out: &mut impl Write,
) -> io::Result<ExitCode> {
    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return Ok(code),
    };
    match workload::run(workload::Options {
        plan: options.plan,
        execute: options.execute,
        base_url: options.base_url,
        operations: options.operations,
        allow_writes: options.allow_writes,
    })
    .await
    {
        Ok(report) => finish_json_report(&report, file, out, options.execute),
        Err(error) => {
            eprintln!("Workload preflight failed: {error}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn finish_json_report(
    report: &serde_json::Value,
    file: Option<output::ReportFile>,
    out: &mut impl Write,
    execute: bool,
) -> io::Result<ExitCode> {
    let published = file.map(|file| file.finish_value(report)).transpose();
    // Keep the terminal copy available even when publishing the file fails.
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(report).expect("report is JSON")
    )?;
    if let Err(error) = published {
        eprintln!(
            "Cannot publish {} report: {error}",
            report["kind"].as_str().expect("report kind")
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(if report["passed"].as_bool().unwrap_or(!execute) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn finish(document: output::Document, file: Option<output::ReportFile>) -> ExitCode {
    if let Some(file) = file
        && let Err(error) = file.finish(&document)
    {
        eprintln!("Cannot save JSON report: {error}");
        return ExitCode::FAILURE;
    }
    if document.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run_profile(
    options: profile::ProfileOptions,
    destination: Option<&Path>,
    out: &mut impl Write,
) -> ExitCode {
    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return code,
    };
    match profile::run(options, out).await {
        Ok(Some(document)) => finish(document, file),
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Profile error: {error}");
            ExitCode::FAILURE
        }
    }
}
