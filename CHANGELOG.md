# Changelog

## Unreleased

- Add reviewed workload JSON plans with runtime actor cookie/custom-header
  bindings, static secret snapshots, bounded binary/environment bodies,
  positive and expected-denial checks, and optional JSON response schemas.
- Add deterministic request mixes, fixed-arrival scheduling, phases, reserved
  group capacity, bounded drain/cancellation, explicit setup outside measurement,
  and explicit write/sequence/replay opt-in with immutable body/key reuse.
- Add schema-v2 workload reports with lifecycle accounting, scoped latency/error
  thresholds, per-request/group/phase metrics, and bounded correlation evidence.
- Allow bounded OpenAPI contract/suite byte-limit overrides while preserving
  parser/schema structural bounds and existing benchmark/suite semantics.
- Encode simple-style path fixture reserved characters as data instead of
  leaving semicolon/comma literal in the target path.
- Cancel wizard prompts on incomplete lines ending at EOF, including whitespace
  or partial affirmative confirmation, without starting requests.

See [workloads](docs/workloads.md). These changes are not in the published
`v0.1.0-alpha.3` package.

## 0.1.0-alpha.3 — 2026-10-26

- Generate deterministic, reviewable suites from raw OpenAPI 3.1 JSON/YAML at
  an HTTP(S) URL or local filesystem path, with bounded parsing and internal
  references.
- Preview without API traffic, then explicitly execute selected GET/HEAD cases
  with fixture bindings, environment authentication, status/media/header/schema
  checks, request budgets, rate pacing, and full-response timeouts.
- Keep writes and unsupported semantics visibly blocked; preserve existing
  benchmark commands and report formats.
- Add acceptance coverage for authentication alternatives and combinations,
  request limits, rate pacing, timeout and response-size failures, and new
  OpenAPI smoke checks for built and extracted release binaries.
- Include the new locked dependencies in generated third-party notices.

See [release notes](docs/releases/v0.1.0-alpha.3.md) and
[OpenAPI suites](docs/openapi-suites.md) for supported scope and limitations.

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
