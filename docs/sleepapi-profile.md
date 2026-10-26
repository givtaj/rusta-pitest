# SleepAPI benchmark profile

This optional profile compares four simulated-wait endpoints on a compatible
SleepAPI server. It describes the requests and response contract expected by
this runner. Start the server separately and check its configuration before
sending a workload; the profile does not deploy or manage it.

## Four comparable endpoints

The profile sends POST requests in this order, completing the client-side run
for one endpoint before starting the next:

1. `/sleep/tokio`: await a timer inside the request task.
2. `/sleep/tokio-spawn`: spawn a timer task, then await its join handle.
3. `/sleep/thread`: sleep an OS thread in Tokio's blocking pool.
4. `/sleep/park`: park an OS thread in that pool, rechecking elapsed time.

Each endpoint receives the same JSON, request count or duration, concurrency,
timeout, and expected status. No `method` field is sent: these named routes
select the method themselves and reject a body containing `method`.

The default payload is `{"duration_ms":400,"status_code":200}`. The default
workload is **100 requests per endpoint**, concurrency **4**, and timeout
**5000 ms**: 400 requests total, about 40 seconds of configured waiting in ideal
conditions. That is an estimate, not a measured benchmark result. There is no
extra health or metadata preflight. Server capacity can be configured lower
than the workload or occupied by other clients. Start with one request per
endpoint when checking a new server.

```sh
# Preview: no server required, no requests sent.
cargo run --release -- profile sleepapi --dry-run

# Run the default workload once the server is available.
cargo run --release -- profile sleepapi

# Choose the server origin or a locally forwarded port.
cargo run --release -- profile sleepapi --base-url http://127.0.0.1:3100

# One request per endpoint for a quick route/status smoke check.
cargo run --release -- profile sleepapi --requests 1 --concurrency 1
```

For guided setup, run `cargo run --release`, choose **6**, select a base origin
and scenario, then choose the delay, run size, timeout, and expected status.
The final review shows all four URLs, payload, and total workload. `q`, `n` at
confirmation, and end-of-input cancel without sending requests.

## Scenarios and expected outcomes

The profile expects these named scenarios on every endpoint:

| `--scenario` | Server delay | Expected HTTP status |
| --- | ---: | ---: |
| `fast_success` | 50 ms | 200 |
| `slow_provider` | 2000 ms | 200 |
| `delayed_failure` | 800 ms | 503 |
| `client_timeout` | 3000 ms | 200, if the client waits long enough |
| `rate_limited` | 100 ms | 429 |
| `invalid_request` | 50 ms | 400 |
| `upstream_timeout` | 2000 ms | 504 |

Selecting a scenario sends `{"scenario":"NAME"}`. `--delay-ms` adds a
`duration_ms` override and accepts zero. The scenario controls the server status,
so `--scenario` and `--status-code` cannot be combined. Without a scenario,
`--status-code` selects a custom server status (200–599 except 204, 205, 206, 226, 304, 407, 416, and 426).
`--expect-status` only changes the runner's assertion; it never changes the
request payload.

```sh
# Expect the intentional HTTP 503, with its normal 800 ms delay.
cargo run --release -- profile sleepapi --scenario delayed_failure

# Treat that same 503 as unexpected; the profile exits with code 1.
cargo run --release -- profile sleepapi --scenario delayed_failure \
  --expect-status 200 --requests 1

# Exercise client timeouts: each timed-out attempt is reported as a failure.
cargo run --release -- profile sleepapi --scenario client_timeout \
  --timeout-ms 500 --requests 1 --concurrency 1

# Compare identical custom responses across all waiting methods.
cargo run --release -- profile sleepapi --delay-ms 250 --status-code 502

# Thirty seconds of load per endpoint: four windows plus final response drain.
cargo run --release -- profile sleepapi --scenario slow_provider \
  --duration-secs 30 --concurrency 8
```

`client_timeout` is a delayed 200 response, not a manufactured HTTP error. The
normal 5000 ms client timeout permits its 3000 ms delay to finish. Set a shorter
client timeout to test abandonment. This remains a failed benchmark (exit `1`),
not a special expected-timeout pass mode. `upstream_timeout` instead completes
an HTTP response with status 504, which can meet the profile's status expectation.

## Reading results

Each route gets a full benchmark report, including request totals, status counts,
timeouts, transport errors, complete and successful throughput, and latency
percentiles. The final summary reports pass/fail for all four routes. Failed
routes do not prevent later routes from running. The process exits `0` only
when all routes pass (or for a valid dry run), `1` for a failed run, and `2` for
invalid CLI configuration. The request count and duration options apply to each
route, not to a shared budget across all four.

These remain closed-loop HTTP/1.1 measurements without warmup or automatic
retries. By default the runner drains response bodies without asserting their
contents. A 429 capacity rejection and an intentional `rate_limited` response
cannot be distinguished by status alone; `--validate-contract` adds that check.
Keep capacity available for baseline comparisons. Fast unexpected errors affect
both latency and complete-response throughput; inspect successful throughput,
statuses, and contract failures when validation is enabled.

After a client timeout, server work may still be active. Started blocking work
retains its capacity slot until it finishes, and a disconnect does not guarantee
immediate handler cancellation. Sequential client runs therefore do not guarantee
that all server work from the previous route has ended. Use small timeout runs
and allow outstanding work to drain before another comparison. The profile does
not prove cancellation behavior or resource usage.

The CLI accepts delays up to 3600000 ms. A delay above the running server's
configured limit can be rejected, even though it is valid CLI input. Check the
server's wait and active-simulation limits; a requested response or scenario
must meet that server's configuration.

## Explicit contract validation

Use the direct profile option to validate **every completed response** without
adding any preflight or postflight requests:

```sh
cargo run --locked --release -- profile sleepapi --validate-contract \
  --requests 1 --concurrency 1 --output contract-smoke.json
cargo run --locked --release -- profile sleepapi --validate-contract \
  --scenario rate_limited --delay-ms 0 --requests 1 --output rate-limited.json
```

Each command sends four POSTs. The wizard remains status-only; generic `get` and
`post` also keep their existing behavior and do not accept this profile flag.
`--dry-run --validate-contract` previews the scope without connecting.

Checks match the **requested simulation**, independently of `--expect-status`:

- Actual HTTP status and body `status_code` match the selected scenario/custom
  status. Overriding `--expect-status` changes only the status assertion; it
  cannot redefine the requested response contract.
- `operation` is `sleep`, `simulated` is `true`, and `requested_duration_ms`
  matches the resolved delay. `method` is respectively `tokio_sleep`,
  `tokio_spawn`, `thread_sleep`, or `thread_park`; `blocks_thread` is false for
  the first two and true for the last two.
- `scenario` equals the selected name, or is absent when no scenario is selected.
- Errors (400–599) contain the status-specific `error.code` and exact boolean
  `error.retryable`. Other responses have no `error` field. Retryable statuses
  are 408, 429, 500, 502, 503, and 504; the full code mapping is in
  [the runner's contract implementation](../src/contract.rs).
- `Retry-After` is exactly one header with value `1` for 429/503, absent otherwise.

A real `capacity_exceeded` response (`simulated: false`) is reported explicitly
as a contract failure, including when an intentional 429 was requested.
Malformed JSON, missing/wrong fields, wrong headers, and oversized bodies fail.
`elapsed_ms`, error messages, and other response headers are not asserted.

Retention is bounded to **65,536 body bytes per in-flight response**, plus JSON
parsing overhead for an accepted-size body. Oversized bodies are still drained
under the original timeout and classified `body_too_large`. Timeouts and
transport/body-read failures remain those failure types; an incomplete response
is not a contract check.

Terminal reports add `Contract checks`, `Contract failures`, and reason counts.
JSON adds `runs[].report.contract_validation` with `checked`, `passed`, `failed`,
`failures` (reason counts), `scope`, and `body_limit_bytes`. A response failing
multiple fields counts once in `failed` and once per reason in `failures`.

HTTP accounting remains independent: `Succeeded` and successful throughput mean
**status matches**, even if a matching response fails its contract. Contract
failures are not added to request totals. A passing endpoint now additionally
requires zero contract failures; any failed endpoint makes the profile exit `1`.
JSON retains all route reports and their failure counts.

Latency is sampled after full body consumption and capture, before JSON parsing
and assertions. Overall elapsed time and throughput include validation overhead.
Use separate small contract checks and leave validation off for resource sweeps;
compare runs with the same validation mode.

## Connecting to the server

`127.0.0.1` refers to the machine running each process. Run both on the same
machine, or establish a reachable private address or SSH tunnel and supply its
HTTP origin with `--base-url`. The profile does not create tunnels, deploy the
API, or open listening ports. A service bound only to loopback on another
machine cannot be reached simply by keeping the default URL.

## Validation scope

Local integration fixtures verify POST routes, JSON bodies, content type,
workload limits, expected statuses, aggregate failures, previews, invalid
configuration, and wizard cancellation. They do not constitute a live benchmark
against SleepAPI or verify its internal waiting mechanisms. Use the running
SleepAPI's `/methods` and `/scenarios` metadata to investigate version or
configuration mismatches.
