use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

use crate::bench::Report;

pub struct RunRecord {
    pub route: Option<&'static str>,
    pub report: Option<Report>,
}

impl RunRecord {
    pub fn passed(&self) -> bool {
        self.report.as_ref().is_some_and(Report::passed)
    }

    fn to_json(&self) -> Value {
        json!({
            "route": self.route,
            "passed": self.passed(),
            "report": self.report.as_ref().map(Report::to_json),
            // Detailed runner errors remain on stderr; do not persist URLs
            // or other request data embedded in third-party error messages.
            "error": self.report.is_none().then_some("runner_error"),
        })
    }
}

pub struct Document {
    pub kind: &'static str,
    pub profile: Option<Value>,
    pub runs: Vec<RunRecord>,
}

impl Document {
    pub fn passed(&self) -> bool {
        !self.runs.is_empty() && self.runs.iter().all(RunRecord::passed)
    }

    fn to_json(&self) -> Value {
        let revision = env!("RUSTA_GIT_REVISION");
        json!({
            "schema_version": 1,
            "runner": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "git_revision": (!revision.is_empty()).then_some(revision),
                "source_dirty": match env!("RUSTA_SOURCE_DIRTY") {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                },
                "rustc_version": env!("RUSTA_RUSTC_VERSION"),
                "target": env!("RUSTA_BUILD_TARGET"),
                "build_profile": env!("RUSTA_BUILD_PROFILE"),
            },
            "kind": self.kind,
            "profile": self.profile,
            "passed": self.passed(),
            "measurement": {
                "protocol": "HTTP/1.1",
                "load_model": "closed_loop",
                "warmup": "none",
                "latency_scope": "all_attempts_through_full_body_or_failure",
                "redirects": false,
                "retries": false,
                "environment_proxies": false,
            },
            "runs": self.runs.iter().map(RunRecord::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Prepare output before any traffic; publish a complete file without replacing
/// an existing report, including one created while the benchmark was running.
pub struct ReportFile {
    destination: PathBuf,
    temporary: PathBuf,
    file: Option<File>,
}

impl ReportFile {
    pub fn prepare(destination: &Path) -> io::Result<Self> {
        // Path components normalize a trailing slash or dot away, but the OS
        // still treats those spellings as directories when publishing a file.
        let spelling = destination.as_os_str().to_string_lossy();
        let filename = spelling
            .rsplit(std::path::is_separator)
            .next()
            .unwrap_or("");
        if matches!(filename, "" | "." | "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a report filename, not a directory",
            ));
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "report already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        for _ in 0..32 {
            let temporary = parent.join(format!(
                ".rusta-pitest-{}-{}.tmp",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => {
                    return Ok(Self {
                        destination: destination.to_owned(),
                        temporary,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temporary report",
        ))
    }

    pub fn finish(mut self, document: &Document) -> io::Result<()> {
        let mut file = self.file.take().expect("prepared report file");
        serde_json::to_writer_pretty(&mut file, &document.to_json())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        // hard_link atomically fails if the destination now exists. Rename
        // would overwrite it on Unix. Both names are on the same filesystem.
        fs::hard_link(&self.temporary, &self.destination)?;
        eprintln!("JSON report: {}", self.destination.display());
        Ok(())
    }
}

impl Drop for ReportFile {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.temporary);
    }
}
