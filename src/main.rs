mod bench;
mod catalogue;
mod cli;
mod contract;
mod output;
mod profile;
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
            || matches!(&cli.command, Some(Command::Profile(options)) if options.dry_run))
    {
        Cli::command()
            .error(
                ErrorKind::ArgumentConflict,
                "--output requires a benchmark; it cannot be used with catalogue or --dry-run",
            )
            .exit();
    }
    let destination = cli.output.as_deref();
    let (method, options, json) = match cli.command {
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
