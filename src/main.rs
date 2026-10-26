mod bench;
mod catalogue;
mod cli;
mod contract;
mod model;
mod openapi;
mod output;
mod profile;
mod schema;
mod strict_json;
mod suite_run;
mod wizard;

use std::{path::Path, process::ExitCode};

use clap::{CommandFactory, Parser, error::ErrorKind};
use reqwest::Method;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
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
            command: cli::OpenApiCommand::Generate { source, out },
        }) => {
            return generate_suite(&source, &out).await;
        }
        Some(Command::Suite {
            command: cli::SuiteCommand::Run(options),
        }) => {
            return run_suite(options, destination).await;
        }
        Some(Command::Catalogue) => {
            catalogue::print();
            return ExitCode::SUCCESS;
        }
        Some(Command::Profile(options)) => return run_profile(options, destination).await,
        Some(Command::Target {
            target,
            base_url,
            mut run,
        }) => {
            let expected_status = *run.expect_status.get_or_insert(target.expected_status);
            let url = target.url(&base_url);
            println!("Target: {} — {}", target.id, target.label);
            println!("{}", target.description);
            println!("GET {url}");
            println!("Expected status: {expected_status}\n");
            (Method::GET, cli::Options { url, run }, None)
        }
        Some(Command::Get(options)) => (Method::GET, options, None),
        Some(Command::Post { options, json }) => (Method::POST, options, json),
        Some(Command::Wizard) | None => match wizard::run() {
            Ok(Some(wizard::Request::Http(method, options, json))) => (method, options, json),
            Ok(Some(wizard::Request::Profile(options))) => {
                return run_profile(options, destination).await;
            }
            Ok(None) => return ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Wizard error: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return code,
    };
    let report = match bench::run(method, options, json).await {
        Ok(report) => {
            report.print();
            Some(report)
        }
        Err(error) => {
            eprintln!("Error: {error}");
            None
        }
    };
    finish(
        output::Document {
            kind: "request",
            profile: None,
            runs: vec![output::RunRecord {
                route: None,
                report,
            }],
        },
        file,
    )
}

async fn generate_suite(source: &str, out: &Path) -> ExitCode {
    let file = match output::ReportFile::prepare(out) {
        Ok(file) => file,
        Err(_) => {
            eprintln!("Cannot prepare suite output; choose a new writable file path");
            return ExitCode::FAILURE;
        }
    };
    match openapi::generate(source).await {
        Ok(suite) => {
            let value = serde_json::to_value(&suite).expect("suite is serializable");
            let bytes = serde_json::to_vec_pretty(&value).expect("suite is serializable");
            if strict_json::parse_json_with_limit(&bytes, 4 * 1024 * 1024 - 1).is_err() {
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

async fn run_suite(options: cli::SuiteOptions, destination: Option<&Path>) -> ExitCode {
    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return code,
    };
    let result = suite_run::run(suite_run::RunOptions {
        suite: options.suite,
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
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("report is JSON")
            );
            let passed = report["passed"].as_bool().unwrap_or(!options.execute);
            if let Some(file) = file
                && file.finish_value(&report).is_err()
            {
                eprintln!("Cannot publish suite report");
                return ExitCode::FAILURE;
            }
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("Suite preflight failed: {error}");
            ExitCode::FAILURE
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

async fn run_profile(options: profile::ProfileOptions, destination: Option<&Path>) -> ExitCode {
    let file = match prepare_report(destination) {
        Ok(file) => file,
        Err(code) => return code,
    };
    match profile::run(options).await {
        Ok(Some(document)) => finish(document, file),
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Profile error: {error}");
            ExitCode::FAILURE
        }
    }
}
