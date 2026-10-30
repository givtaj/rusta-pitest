# rusta-pitest

A small Rust CLI for checking HTTP APIs and measuring response times. Send GET
and POST requests to your own HTTP/HTTPS service, run concurrent workloads, and
save versioned JSON reports. Optional Roots and SleepAPI presets are included;
neither service is required to use the runner.

## Install the alpha

`v0.1.0-alpha.5` bounds decoded JSON/YAML input and expanded schema references
for OpenAPI suites and authenticated, fixed-arrival workloads. CLI options and the
JSON report schema may change in later alpha versions. The supported binaries are
Linux x86_64 (Ubuntu 24.04 or a compatible glibc environment) and macOS arm64.
Other operating systems and architectures are not verified for this release.

Download the archive for your platform and `SHA256SUMS` from the
[GitHub release](https://github.com/givtaj/rusta-pitest/releases/tag/v0.1.0-alpha.5).
Verify the downloaded archive before extracting it:

```sh
# macOS; on Linux, use sha256sum instead of shasum -a 256.
shasum -a 256 rusta-pitest-v0.1.0-alpha.5-*.tar.gz
```

Compare the complete digest and filename with the matching line in
`SHA256SUMS`, then extract the archive and run its `rusta-pitest --version` and
`rusta-pitest --help`. Place the executable in a directory on your `PATH` if
you want to run it from any directory. macOS binaries are not signed or notarized.
Each binary archive includes its third-party license notices.

Alternatively, install from the exact release tag using a current stable Rust
toolchain:

```sh
cargo install --locked --git https://github.com/givtaj/rusta-pitest.git \
  --tag v0.1.0-alpha.5 rusta-pitest
rusta-pitest --version
```

This alpha is distributed through GitHub; no crates.io publication is required.
The repository development examples below use `cargo run --release -- ...`;
after installation, use `rusta-pitest ...` with the same arguments.

See the [release notes](docs/releases/v0.1.0-alpha.5.md),
[release thresholds](docs/release-policy.md), and [changelog](CHANGELOG.md).
The release thresholds govern release quality. Authored workloads additionally
provide per-run latency and failure thresholds; direct benchmarks and generated
suites retain their existing pass/fail semantics.
Release checks include `cargo-audit` 0.22.2 with `cargo audit --deny warnings`;
known vulnerabilities and denied warnings block publication. Final validation
and artifact evidence are recorded in the published GitHub release.

## Try it locally

Start a temporary HTTP server in an empty directory, bound to loopback only:

```sh
demo_dir=$(mktemp -d)
python3 -m http.server 8000 --bind 127.0.0.1 --directory "$demo_dir"
```

Keep that terminal open. In another terminal, send a single request and then a
small benchmark:

```sh
rusta-pitest get http://127.0.0.1:8000/ --expect-status 200
rusta-pitest get http://127.0.0.1:8000/ --requests 20 --concurrency 2 \
  --expect-status 200 --output local-demo.json
```

Both commands should exit `0`; the second saves a new JSON report. Use a new
output filename on repeat runs because existing reports are never overwritten.
Press Ctrl-C in the server terminal when finished. This server is a convenient
functional fixture, not a performance baseline. The examples require Python 3;
the runner itself does not.

## OpenAPI contract suites

Generate a reviewable suite from a raw OpenAPI 3.1 JSON/YAML URL or local path,
then preview it without API traffic:

```sh
rusta-pitest openapi generate ./openapi.yaml --out /tmp/api.suite.json
rusta-pitest suite run /tmp/api.suite.json
```

Execution requires `--execute`, an explicit `--base-url`, and selected
`--operation` IDs. The initial runner permits GET/HEAD only, with fixture IDs
and bearer/header API-key credentials bound at runtime. Writes and unsupported
semantics remain visibly blocked. See [OpenAPI suites](docs/openapi-suites.md)
for a [synthetic example](examples/widgets.openapi.yaml), execution, supported
contracts, and coverage reports.

## Authored workloads

Introduced in `v0.1.0-alpha.4`, `workload run` previews an authored plan without
traffic or reading secret environment values:

```sh
cargo run --locked -- workload run examples/workload.json
```

The runner adds isolated actor cookie/custom-header bindings, fixed-arrival
request mixes, measured phases, reserved group capacity, explicit request
sequences and opted-in writes/replay. It records offered, started, completed,
failed, cancelled and unsent requests, scoped thresholds, per-route latency,
and bounded timing evidence. There is no request queue, automatic retry, or
automatic authentication renewal. Execution requires `--execute`, an explicit
`--base-url`, every referenced `--operation`, and `--allow-writes` for writes.
See [workloads](docs/workloads.md) for a runnable localhost fixture, the complete
plan, credential handling, sequence semantics, limits, and schema-v2 reports.

From alpha.4, OpenAPI commands also accept bounded larger inputs:
`openapi generate --max-input-bytes` defaults to 1 MiB, its
`--max-suite-bytes` defaults to 4 MiB, and `suite run --max-input-bytes`
defaults to 4 MiB. Each accepts 1–67108864 bytes; structural bounds still apply.
See [OpenAPI input limits](docs/openapi-suites.md#generate-and-review).

## Guided mode

From this repository, open the wizard:

```sh
cargo run --release
```

Choose **1 — Roots greeting**, **2 — Roots database health**, **3 — Custom GET**,
**4 — Custom POST**, **5 — Roots catalogue**, or **6 — SleepAPI benchmark profile**.
The Roots catalogue lets you select a target by number (1–9) or ID, then choose
its base origin. Custom requests ask for a URL and optional one-line JSON body.
The SleepAPI profile compares all four waiting endpoints with the same scenario.
Every path then offers one request or a benchmark by request count or duration,
followed by timeout, expected status and a final review.

Press Enter to accept the displayed defaults. Type `q` at any prompt, answer
`n` at the final review, or close input to cancel without sending requests.
An actual blank line accepts a default; end-of-input cancels.

The Roots shortcuts use `http://127.0.0.1:4301`. Use the catalogue's base-origin
prompt or a custom URL for a different host or port. You can also open the wizard
explicitly with
`cargo run --release -- wizard`.

## Roots catalogue

List the nine built-in GET targets without sending any requests:

```sh
cargo run --release -- catalogue
```

`catalog` is an alias. Run a target with its exact expected-status default:

```sh
cargo run --release -- target roots-health
cargo run --release -- target roots-strains-signed-out
cargo run --release -- target roots-calendar-signed-out \
  --base-url http://127.0.0.1:5301
```

The catalogue covers greeting, database health, OpenAPI, Swagger, and signed-out
checks for sessions, customer profiles, strains, journal history and the calendar.
The signed-out targets expect **401** and check access protection; they do not
measure successful authenticated reads. All presets send GET requests without
cookies or CSRF headers. See the [Roots catalogue](docs/roots-catalogue.md) for
IDs, paths, source evidence and future authenticated cases.

These presets describe the requests sent by this runner; confirm that your
running service exposes the same routes. They do not establish successful
authenticated access or the health of an unrelated deployment.

## SleepAPI benchmark profile

Run the same workload against all four SleepAPI waiting endpoints, in sequence:

```sh
# Preview the four requests and workload; sends no traffic.
cargo run --release -- profile sleepapi --dry-run

# 100 requests per endpoint, concurrency 4, 400 ms delay, expected HTTP 200.
cargo run --release -- profile sleepapi

# Same seven named scenarios as SleepAPI; this one expects HTTP 503.
cargo run --release -- profile sleepapi --scenario delayed_failure

# A 30-second request window for EACH endpoint (at least 120 seconds total).
cargo run --release -- profile sleepapi --scenario slow_provider \
  --duration-secs 30 --concurrency 8
```

| Endpoint | Waiting mechanism |
| --- | --- |
| `POST /sleep/tokio` | Timer awaited in the request task |
| `POST /sleep/tokio-spawn` | Timer in a spawned task, then joined |
| `POST /sleep/thread` | Thread sleep in the blocking pool |
| `POST /sleep/park` | Thread parking in the blocking pool |

Use wizard option **6** for a guided setup and review. Each endpoint gets its own
latency/throughput report, followed by a four-endpoint pass/fail summary. The
profile continues after an endpoint fails and exits with `1` if any endpoint
fails. A passing failure scenario means the expected error status was received.

| Profile option | Default | Meaning |
| --- | --- | --- |
| `--base-url` | `http://127.0.0.1:3000` | SleepAPI origin, with no path or credentials |
| `--requests`, `-n` | `100` | Requests **per endpoint**, 400 total by default |
| `--duration-secs` | None | Request window **per endpoint**; conflicts with explicit `--requests` |
| `--concurrency`, `-c` | `4` | Concurrent requests within the current endpoint |
| `--timeout-ms` | `5000` | Full-response client timeout; timeouts count as failures |
| `--scenario` | None | Named preset applied to all four endpoints |
| `--delay-ms` | `400`, or scenario delay | Override the server wait, including zero |
| `--status-code` | `200` | Custom server response status; conflicts with `--scenario` |
| `--expect-status` | Scenario/custom status | Override the exact HTTP status required for a pass |
| `--validate-contract` | Off | Assert JSON simulation fields and Retry-After on every completed response |
| `--dry-run` | Off | Print the workload and URLs without connecting |

See [SleepAPI profile details](docs/sleepapi-profile.md) for all seven scenarios,
timeout experiments, capacity limits, and running across the Mac/Linux boundary.
SleepAPI must already be running; this command does not launch it.

## Direct commands

From this repository, check the local demo server:

```sh
cargo run --release -- get http://127.0.0.1:8000/
```

Repeat it with four concurrent workers:

```sh
cargo run --release -- get http://127.0.0.1:8000/ \
  --requests 100 --concurrency 4 --expect-status 200
```

Run a ten-second request window at concurrency two:

```sh
cargo run --release -- get http://127.0.0.1:8000/ \
  --duration-secs 10 --concurrency 2 --expect-status 200
```

In the wizard, choose **3 — Custom GET**, enter the local URL, then select a
request-count or duration workload and review the final settings before running.

For an API that provides a POST endpoint:

```sh
cargo run --release -- post http://127.0.0.1:3000/echo \
  --json '{"message":"hello"}' --requests 10 --concurrency 2
```

The echo URL is an example requiring a server that accepts POST; Python's simple
HTTP server does not provide it. Direct benchmark commands do not support custom
headers or session cookies. OpenAPI suites provide contract-defined scalar headers
and bearer/header API-key authentication. POST behavior is verified by a local server in the integration
tests. Omitting `--json` sends an empty POST;
providing it validates the JSON before sending and sets
`Content-Type: application/json`. Each repetition sends the same body again.

## Options

`get`, `post`, and `target` share these run options:

| Option | Default | Meaning |
| --- | --- | --- |
| `--requests`, `-n` | `1` | Total requests when no duration is given |
| `--duration-secs` | None | Seconds to keep starting requests; conflicts with explicit `--requests` |
| `--concurrency`, `-c` | `1` | Maximum simultaneous requests; capped at request count only in count mode |
| `--timeout-ms` | `5000` | Timeout through the full response body |
| `--expect-status` | Any 2xx for `get`/`post`; preset status for `target` | Require an exact HTTP status; an explicit value overrides a preset |

Only `post` accepts `--json`. `target` also accepts `--base-url`, defaulting to
`http://127.0.0.1:4301`; supply an HTTP/HTTPS origin with no path, query, fragment
or credentials. Use `--help`, `get --help`, `post --help`, or `target --help` for
the command reference.

Exit codes: `0` when every request meets its status and enabled contract checks,
a profile dry run completes, or the wizard is cancelled; `1` for failed checks
or runner/report errors; `2` for invalid command-line arguments. Generic requests
check status codes. SleepAPI profiles can additionally assert response contents
with `--validate-contract`; any contract failure exits `1`.

## Saving JSON reports

Add `--output FILE` to save a versioned JSON report alongside the normal terminal
output. Each report contains the workload, build identity, timestamps, status and
failure counts, full-precision elapsed time and throughput, and latency quantiles.

```sh
cargo run --locked --release -- get http://127.0.0.1:8000/ --requests 20 \
  --concurrency 2 --output local-demo.json
cargo run --locked --release -- profile sleepapi --output sleepapi.json
cargo run --locked --release -- --output guided-run.json wizard
```

Failed benchmarks also save reports and retain exit code `1`. Existing files are
never overwritten; use a new filename for each run. Output preparation happens
before traffic. Wizard cancellation creates no report. `catalogue` and
`profile --dry-run` cannot be combined with `--output`.

Saved URLs omit credentials, queries, and fragments; request bodies are omitted.
See [JSON report format](docs/json-reports.md) for the schema, units, build
metadata, and output behavior.

## Reading a benchmark

Start at concurrency 1–4 and increase cautiously while observing the client and
target service. No maximum safe concurrency or capacity has been verified; the
CLI's accepted argument range is not a capacity guarantee.

The terminal report includes request counts, successful responses, unexpected
statuses, timeouts, transport/body errors, elapsed time, throughput, and
p50/p95/p99 latency.

- A request finishes when its body has been fully consumed, or when it fails.
  Bodies are drained without retaining the entire response in memory.
- Every attempted request contributes one latency sample, including failures.
  Reported HTTP status counts include only responses whose bodies completed.
- Throughput counts complete HTTP responses per second, including unexpected
  statuses; successful throughput counts only responses meeting the expectation.
  Both use the total run time, including failures and runner overhead.
- `hdrhistogram` records microseconds with three significant digits; percentiles
  are approximate. Small runs are smoke checks, not stable performance baselines.
- Workers send their next request after the previous one finishes. This is
  **closed-loop** load, so slower responses reduce the request rate.
- With `--duration-secs`, all workers share the same deadline. They stop starting
  new requests at that deadline and let in-flight responses finish under their
  request timeout. Total elapsed time can exceed the requested window; latency
  and throughput include those final requests and the time spent finishing them.
- There is **no warmup** yet. Connection setup and TLS handshakes are included in
  the first requests. The timer starts after client/request preparation and ends
  after all workers finish and their results are combined.
- HTTP/1.1 connections are reused. Redirects and automatic retries are disabled,
  and requests connect directly without environment-configured proxies.
- An interrupted process does not produce a completed benchmark report.

Use release builds and repeat measurements under comparable conditions before
drawing performance conclusions. For background, see the
[reqwest client documentation](https://docs.rs/reqwest/latest/reqwest/struct.Client.html),
[HDR histogram documentation](https://docs.rs/hdrhistogram/latest/hdrhistogram/),
and [open versus closed load models](https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/).

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
```

The integration tests use temporary loopback servers and do not require Roots,
PostgreSQL, or external network access.

GitHub Actions runs these checks on pushes and pull requests. Regression tests
cover persistent HTTP connections, full-body failures, mixed outcomes, JSON
artifacts, source provenance, and release notice generation.

A small build script records compiler and source identity.
Source archives without their own Git metadata report unknown revision and
source cleanliness (`null` in JSON), even inside an unrelated Git checkout.
Official release binaries carry their tagged source revision and clean marker.
The separate workload runner supports authored plans and static
actor authentication. Automatic session renewal, target lifecycle management,
warmup controls, and saved comparisons remain separate work.

Contributions and bug reports are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md)
for the development workflow. Report suspected vulnerabilities through the
private channel in [SECURITY.md](SECURITY.md). The project is licensed under
[Apache 2.0](LICENSE); release archives also carry dependency license notices.
