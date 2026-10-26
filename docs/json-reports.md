# JSON benchmark reports, schema version 1

Check both `schema_version` and `kind` when consuming reports. This document
describes `kind: request` and `kind: profile`. OpenAPI suite reports use
`kind: suite` and a separate preview/execution shape described in
[OpenAPI suites](openapi-suites.md#reports-and-editable-suites).

Use `--output FILE` before or after a command to save one JSON document while
keeping the normal terminal output. It works with `get`, `post`, `target`,
`profile sleepapi`, and confirmed wizard runs.

```sh
cargo run --locked --release -- get http://127.0.0.1:4301/api/hello \
  --requests 100 --concurrency 4 --expect-status 200 --output hello.json
cargo run --locked --release -- profile sleepapi --scenario delayed_failure \
  --requests 1 --output expected-503.json
```

The second example can pass: `succeeded` means a completed response matched the
expectation, including an explicitly expected error status. With SleepAPI
`--validate-contract`, status matches are independent of contract failures;
`passed` also requires zero contract failures. Client timeouts
remain failures. A report is saved for failed benchmarks as well as passing ones;
the existing benchmark exit codes remain unchanged. Failure to save exits `1`.

## Document fields

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer `1`; consumers should check this before parsing |
| `runner` | Build identity described below |
| `kind` | `request` or `profile` |
| `profile` | `null` for direct/target requests; SleepAPI name, scenario (or `null`), resolved `delay_ms` and `status_code` otherwise |
| `passed` | True only if every run passed |
| `measurement` | HTTP protocol, closed-loop load model, no warmup, full-body/failure latency scope, and disabled retries, redirects, and environment proxies |
| `runs` | One entry for a direct/target request; four ordered entries for SleepAPI |

Each `runs` entry contains `route` (`null` for direct/target requests), `passed`,
`report`, and `error`. Ordinarily `error` is `null`. A runner setup/internal error
has `report: null`, `passed: false`, and `error: "runner_error"`; its diagnostic is
on stderr, without copying potentially sensitive error text into the artifact.
Other profile endpoints still run. HTTP failures, body failures, and timeouts
are ordinary measured outcomes with a non-null report.

## Per-run report fields

- `request`: HTTP `method`, sanitized `url`, `query_redacted`, and `body_bytes`.
  User information, all query parameters, and fragments are omitted from saved
  URLs. Bodies are not stored. Keep any inputs needed to reproduce the request
  separately; this artifact is not a replay file. URL paths remain visible.
- `workload`: `mode` (`requests` or `duration`), configured `requests` or
  `duration_secs` (the other is `null`), `requested_concurrency`,
  `effective_concurrency`, `timeout_ms`, and `expect_status`.
  A null expectation means any 2xx response; catalogue defaults are resolved
  into their exact status. Count runs cap effective concurrency at request count.
- `started_at_unix_ms` / `finished_at_unix_ms`: UTC wall-clock timestamps in
  milliseconds since the Unix epoch. Elapsed timing uses a monotonic clock;
  use `metrics.elapsed_secs` for timing, not timestamp subtraction.
- `metrics`: the fields below. Values come directly from the runner, without
  parsing or rounding the terminal report.

| Metric | Meaning / unit |
| --- | --- |
| `attempted` | Number of measured attempts |
| `succeeded` | Complete responses matching the status expectation |
| `unexpected_status` | Complete responses not matching the expectation |
| `timeouts` | Requests that timed out, including while reading the body |
| `transport_errors` | Other request/body failures |
| `complete_responses` | `succeeded + unexpected_status` |
| `status_counts` | Object mapping HTTP status strings to counts; excludes incomplete bodies |
| `elapsed_secs` | Floating-point seconds, including finishing in-flight requests and runner overhead |
| `complete_responses_per_sec` | `complete_responses / elapsed_secs` |
| `successful_responses_per_sec` | `succeeded / elapsed_secs` |
| `latency.samples` | One sample per attempted request |
| `latency.p50_us`, `p95_us`, `p99_us` | Integer microseconds from the HDR histogram; approximate quantiles across all attempts, including failures |

Accounting identities:

```text
attempted = succeeded + unexpected_status + timeouts + transport_errors
attempted = latency.samples
complete_responses = sum(status_counts.values())
```

## Build identity

`runner` includes `name`, package `version`, `git_revision`, `source_dirty`,
`rustc_version`, `target`, and `build_profile` (`debug` or `release`). Values are
captured when compiling, not by inspecting the checkout when the binary runs.
Git fields are `null` when Git metadata is unavailable. `source_dirty` covers
`Cargo.toml`, `Cargo.lock`, `build.rs`, and `src`; documentation/test edits alone
do not mark the executable's source dirty. A dirty flag does not identify the
exact changes: commit source before collecting a reproducible baseline.
This metadata describes the runner, not the server being measured.

## File behavior

The destination must be a new filename in an existing writable directory.
Preparation fails before sending requests for an existing destination or an
unavailable parent. The completed report is published atomically without
replacing a file that appeared during the run. This uses a temporary file and
hard link in the same directory; the destination filesystem must support hard
links. A failure during final writing/publication exits `1` and leaves existing
files intact; the terminal report remains available.

Cancelled wizard runs create no files. `--output` with `catalogue` or
`profile --dry-run` is rejected with exit `2` because neither produces measured
results. An interrupted process does not publish a completed report and may
leave a `.rusta-pitest-*.tmp` file in the destination directory.

## Optional SleepAPI contract results

`profile sleepapi --validate-contract` adds `contract_validation` to each
non-null per-run report. This is an additive version-1 field, omitted when
validation is disabled. It contains `scope: "all_completed_responses"`,
`body_limit_bytes: 65536`, and counts `checked`, `passed`, `failed`, plus a
`failures` object mapping reason names to counts. `checked = passed + failed`;
reason counts can sum above `failed` when a response violates several fields.
Incomplete responses are not checked; they remain timeout/transport failures.

Contract counts are separate from HTTP metrics and do not change the existing
request accounting identities. A response can increment `metrics.succeeded`
while failing its contract and setting its run/document `passed` to false.
Raw bodies are never saved. See [contract validation](sleepapi-profile.md#explicit-contract-validation)
for asserted fields, retained-body limits, and timing scope.
