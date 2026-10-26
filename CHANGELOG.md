# Changelog

## 0.1.0-alpha.2 — 2026-10-26

First public alpha of the Rust HTTP API test and benchmark CLI.

- Send GET and POST requests, including a repeated JSON body.
- Run request-count or duration workloads with concurrency, full-response
  timeouts, and exact expected-status checks.
- Use a guided wizard, optional Roots targets, and SleepAPI profiles with
  response contract validation.
- Report success, unexpected statuses, timeouts, transport/body failures,
  throughput, and approximate p50/p95/p99 latency.
- Save versioned JSON reports with build provenance and existing-file protection.
- Ship Linux x86_64 and macOS arm64 archives with checksums and generated
  third-party license notices.
- Provide a standalone localhost quickstart, contribution instructions, and
  private vulnerability reporting.
- Enforce formatting, strict Clippy, regression tests, release-binary smoke
  checks, dependency audits, and artifact verification before publication.

See [release notes](docs/releases/v0.1.0-alpha.2.md) for supported scope and
limitations. No throughput or target-service capacity guarantee is implied.
