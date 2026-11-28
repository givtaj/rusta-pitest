# Changelog

## 0.1.0-alpha.9 — 2026-11-28

- Bind private typed JSON values per actor during workload preflight. Use them
  in structured path segments, query values, and expected response fields.
  Preview checks declarations and references without loading private values.
- Compare expected JSON values and unordered scalar sets with `expect_fields`.
  A measured read with HTTP 200 and a valid response schema still fails with
  `response_field_mismatch` when the actor's expected tenant set differs.
- Preserve validated numeric spelling in URL fixtures and require exactly
  represented integers in numeric response expectations. Use JSON strings for
  identifiers that are fractional or outside the supported integer range.
- Exercise per-actor bindings, a credential swap, the schema-only control, and
  redaction in the release-binary smoke suite.

See the [alpha.9 release notes](docs/releases/v0.1.0-alpha.9.md) for compatibility
and the [workload guide](docs/workloads.md#private-actor-fixtures) for a loopback
example.

## 0.1.0-alpha.8 — 2026-11-25

- Add `workload capacity` to find a repeatedly passing request rate and burst
  size for one actor and one GET or HEAD operation. Preview the plan without
  sending traffic. Execution requires an explicit base URL and operation.
- Exclude warmup, stop each search independently at its first failure or
  configured ceiling, and repeat the last passing level twice. Plans can
  require a maximum p95 latency for successful reads.
- Report status counts, unsent reasons, peak client requests in flight, and
  successful reads per second through drain. Separate successful-read latency
  from latency across all attempts so fast errors are visible.
- Update the locked `url` dependency to 2.5.9 and `form_urlencoded` to 1.2.3.

See the [alpha.8 release notes](docs/releases/v0.1.0-alpha.8.md) for the
report format and a bounded loopback example.

## 0.1.0-alpha.7 — 2026-11-18

- Reject unknown `--fixture` names during OpenAPI suite preflight, before any
  request can use a generated value in place of the intended fixture.
- Check string response headers literally, including quotes and backslashes.
- Save the full `--output` report and preserve the runner's exit status when a
  reader such as `head` closes stdout early.
- Stop the workload throughput timer when the drain finishes. Final accounting,
  threshold evaluation, and report generation no longer reduce the reported
  completion rate.
- Recheck the phase deadline before starting a new arrival. Count expired
  arrivals as unsent with reason `scheduler_late`.
- Use the bundled `examples/widgets.openapi.yaml` in the README quickstart and
  create a fresh output directory on each run.

See the [alpha.7 release notes](docs/releases/v0.1.0-alpha.7.md) for examples
and guidance on comparing results with earlier versions.

## 0.1.0-alpha.6 — 2026-11-17

- Add bounded, ordered literal query pairs to authored workloads, preserving
  repeated names and empty values with exact UTF-8 percent encoding.
- Reject malformed or oversized queries before setup or measured traffic and
  omit query names/values from reports and diagnostics. Existing plans and
  workload/report versions remain unchanged.
- Bind declared OpenAPI cookie security schemes privately through `--auth-env`,
  preserving selected GET/HEAD execution and security alternatives/combinations.
- Validate cookie names and opaque values before traffic, reject conflicting
  cookie bindings, and serialize combined cookies deterministically.
- Cover isolated synthetic users, denied positive cases, preview without secret
  resolution, and credential redaction in regressions and release-binary smokes.
- Update the locked `quinn-udp` dependency to 0.5.16, addressing
  RUSTSEC-2026-0538 found by the fresh release audit.

See [release notes](docs/releases/v0.1.0-alpha.6.md) for compatibility and
the static authentication and query boundaries.

## 0.1.0-alpha.5 — 2026-10-29

- Bound decoded string and object-key bytes during JSON/YAML parsing, including
  YAML alias expansion, using the configured input budget.
- Reject schemas whose expanded reference traversal exceeds the supported
  complexity budget before compilation or request execution, while retaining
  productive recursion and reference sibling constraints.
- Check recursion on every expanded reference path so reusing a cached schema
  cannot admit a cycle that stays on the same instance value.

See [release notes](docs/releases/v0.1.0-alpha.5.md) for compatibility and
remaining resource-limit boundaries.

## 0.1.0-alpha.4 — 2026-10-28

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
- Preserve completed request outcomes when the final drain expires rather than
  misclassifying already finished tasks as cancelled.
- Harden temporary test-fixture creation with exclusive opens and private
  permissions, preserving pre-existing files and refusing symlinks.

See [release notes](docs/releases/v0.1.0-alpha.4.md) and
[workloads](docs/workloads.md) for supported scope and limitations.

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
