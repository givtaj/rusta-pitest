# Authored workloads

Introduced in `v0.1.0-alpha.4`, `workload run` executes reviewed JSON plans.
Examples below use `cargo run` from a source checkout; an installed executable
uses `rusta-pitest` with the same arguments. Binary archives include `docs/` and
`examples/` for local reference and preview.
Existing direct benchmarks remain closed loop and generated OpenAPI suites
remain once-per-selected-GET/HEAD contract checks. A workload is a separate,
reviewed JSON plan, not an OpenAPI importer or a recorded production session.

## Try a synthetic local workload

Save this fixture as `/tmp/rusta-workload-demo.py`, then start it in one terminal:

```python
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json

class Demo(BaseHTTPRequestHandler):
    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/denied":
            return self.reply(401, {"denied": True})
        cookie = self.headers.get("Cookie", "")
        actor = cookie.removeprefix("session=")
        allowed = actor in ("alice", "bob", "canary")
        allowed &= self.headers.get("X-CSRF-Token") == "csrf-" + actor
        self.reply(200 if allowed else 403, {"ok": bool(allowed)})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.reply(200, {"ok": True})

    do_PUT = do_PATCH = do_DELETE = do_POST

    def log_message(self, *args):
        pass

ThreadingHTTPServer(("127.0.0.1", 8001), Demo).serve_forever()
```

```sh
python3 /tmp/rusta-workload-demo.py
```

The fixture needs Python 3.9 or newer. It accepts synthetic writes but does not
implement idempotency or model application capacity. Stop it with Ctrl-C when
finished. The runner does not start, reset, supervise, or stop the target server;
the caller or an external harness owns those tasks and readiness checks.

In another terminal, from the repository, preview the
[complete example](../examples/workload.json). Preview sends no HTTP requests,
does not read secret environment values or body files, and has `passed: null`:

```sh
cargo run --locked -- workload run examples/workload.json
cargo run --locked -- workload run examples/workload.json \
  --base-url http://127.0.0.1:8001
```

Supply the synthetic actor credentials and explicitly select every referenced
request, including setup, before executing:

```sh
export RUSTA_DEMO_COOKIE_A='session=alice' RUSTA_DEMO_CSRF_A='csrf-alice'
export RUSTA_DEMO_COOKIE_B='session=bob' RUSTA_DEMO_CSRF_B='csrf-bob'
export RUSTA_DEMO_COOKIE_C='session=canary' RUSTA_DEMO_CSRF_C='csrf-canary'
cargo run --locked --release -- workload run examples/workload.json \
  --execute --base-url http://127.0.0.1:8001 \
  --operation health --operation read --operation denied \
  --output /tmp/workload-demo.report.json
```

Use a new report filename for each run. The example offers 32 measured requests
over five seconds, plus one separate setup request. `baseline` is a measured
phase, not an excluded warmup. The expected 401 is a passing denial case.
All selected actor/request pairs pass preflight before setup or measured traffic.
An invalid binding blocks the run rather than sending a partial selection.
Successful preview exits zero without claiming a measured pass. Execution exits
zero only when `passed` is true; invalid input, preflight, threshold/measurement,
or report-publication failures are nonzero.

## Plan shape and request bindings

`workload_version` is `1`; unknown fields and duplicate JSON keys are rejected.
Actor, request, group, and phase IDs use 1–64 ASCII letters, digits, `_`, `-`,
or `.`. IDs remain visible in reports, so use non-sensitive names.

| Field | Meaning |
| --- | --- |
| `actors` | Map of actor IDs to `{"headers_env":{"Header-Name":"ENV_NAME"}}`; `{}` is an anonymous actor |
| `requests` | Map of named request specifications, described below |
| `groups` | Each group has distinct `actors`, `max_in_flight`, and exactly one of `requests` or `sequence` |
| `phases` | Ordered `name`, positive `duration_ms`, and `rates` mapping group IDs to offered arrivals per second; omitted/zero rates offer no traffic |
| `setup` | Optional ordered `{"actor":"alice","request":"health"}` entries, executed once before the measured clock starts |
| `max_in_flight` | Global bound, default `16`; group reservations must fit inside it |
| `timeout_ms`, `drain_timeout_ms` | Full-response request timeout and final drain bound; each defaults to `5000` |
| `max_start_lag_ms` | Maximum allowed start lateness, default `20`; expired arrivals are skipped |
| `thresholds` | Optional maximum-based checks with request/group/phase scope |
| `evidence` | Optional `max_records` and `max_bytes`; defaults `10000` and `2097152` |

A request requires `method` and a slash-prefixed `path`, such as `/widgets/one`.
The explicit `--base-url` may include a path prefix; `/api/v1` plus `/widgets`
sends `/api/v1/widgets`. Base credentials, queries and fragments are rejected.
Absolute/authority paths, dot segments, encoded path separators, malformed
percent escapes, queries, and fragments in request paths are rejected. Supply
already percent-encoded path data where necessary; no response extraction or
dynamic URL templating occurs.

Optional request fields are `expect_status`, `response_schema`, `body_env`,
`body_file`, `content_type`, and `idempotency_key_env`. Without `expect_status`,
any 2xx status passes; set an exact `401` or `403` for a denial case. An optional
inline `response_schema` checks JSON using the supported schema subset and local
references, without external retrieval. It does not assert media type or JSON
`format`. HEAD is bodyless and cannot have a response schema.
The same [schema admission limits](openapi-suites.md#assertions-and-coverage) apply
to workload response schemas; unsupported or over-budget schemas reject the
plan during preflight, before setup or measured traffic. HTTP timeouts cover
the response transfer, not synchronous schema validation.

Header bindings contain complete header values: an `Authorization` environment
value must include its `Bearer ` prefix. Cookie and CSRF values are supplied
separately for each actor. Transport/host overrides and case-insensitive header
collisions are refused, including collisions with `content_type` or
`idempotency_key_env`. There are at most 64 headers, 8192 bytes per value, and
65536 total header-name/value bytes per prepared request.

Bindings are snapshots loaded once before setup. Cookie values are static;
`Set-Cookie` is ignored. There is no automatic login, cookie jar, token refresh,
or authentication retry. Setup cannot extract a token or update already prepared
credentials or bodies. Acquire credentials, reset fixtures, and perform any
renewal through a separately controlled harness before launching a new run.
Setup results have `measured: false` and do not enter measured counters/latencies;
a failed setup prevents measured requests from starting.

## Sequences and deliberate writes

GET/HEAD execute without write permission. POST/PUT/PATCH/DELETE additionally
require `--allow-writes`, along with `--execute`, the target, and every referenced
`--operation`. Preview can review writes before granting permission and reports
`requires_write_opt_in`. Other methods are unsupported. Writes are never inferred
from an imported OpenAPI example.

For a write demonstration, copy the complete example to a new plan and replace
its `requests`, `groups`, `setup`, `phases`, and `thresholds` fields with this
fragment. Keep its actor bindings and global bounds. Indices in `replay_of` are
zero based:

```json
{
  "requests": {
    "write": {
      "method": "POST", "path": "/write",
      "body_env": "RUSTA_DEMO_BODY", "content_type": "application/json",
      "idempotency_key_env": "RUSTA_DEMO_KEY", "expect_status": 200
    }
  },
  "groups": {
    "writes": {
      "actors": ["alice"], "max_in_flight": 1,
      "sequence": [{"request": "write"}, {"request": "write", "replay_of": 0}]
    }
  },
  "setup": [],
  "phases": [{"name": "write-demo", "duration_ms": 1000, "rates": {"writes": 1}}],
  "thresholds": [{"metric": "error_rate", "maximum": 0}, {"metric": "unsent_rate", "maximum": 0}]
}
```

Bind `RUSTA_DEMO_BODY='{"value":"synthetic"}'` and
`RUSTA_DEMO_KEY='synthetic-logical-write-1'`, then select `--operation write`
and add `--allow-writes`:

```sh
export RUSTA_DEMO_BODY='{"value":"synthetic"}' RUSTA_DEMO_KEY='synthetic-logical-write-1'
cargo run --locked --release -- workload run /tmp/write-workload.json \
  --execute --base-url http://127.0.0.1:8001 --operation write --allow-writes \
  --output /tmp/write-workload.report.json
```

Use the path where you saved the edited plan. Alternatively, use `body_file` for exact binary bytes:
its relative path is resolved beneath the plan directory, cannot traverse out
through `..` or symlinks, and must be a regular file. Choose one body binding;
bodies are capped at 65536 bytes (64 KiB). GET/HEAD bodies are refused.

Each sequence runs serially on its selected actor, retaining its reserved slot
until it terminates. A failed child leaves later children `unsent` with
`dependency_failed`. A replay must name the same request as an earlier step and
have an idempotency-key binding. It reuses identical prepared body bytes and key.
The supplied key stays unchanged across **all** repetitions of that named
request, including later sequences and actors. The runner does not generate a
new logical-write key per arrival; author separate request/body/key bindings if
distinct writes are needed. The server decides whether a key deduplicates a
write; replay is not proof of exactly-once behavior.

## Scheduling and bounds

An ordinary group's `requests` list is a deterministic round-robin mix; repeated
names give that request additional turns. Actors also rotate deterministically.
For a sequence group, each offered arrival means one whole sequence; report
request counters include all of its children, with separate `sequence_counts`.

Arrivals use monotonic deadlines independent of response completion, starting
at phase offset zero and excluding the phase end. A phase offers
`ceil(duration_ms * rate / 1000)` arrivals per active group. Slowing the server
does not reduce the offered count. Each actor has one active request/sequence
and belongs to one group; group capacity is reserved and cannot be borrowed.
This permits a canary group to retain capacity while another group is busy.

There is no waiting queue, catch-up burst, redirect, automatic retry, or
environment proxy. An arrival older than `max_start_lag_ms`, or already
superseded by its next scheduled arrival, becomes `unsent: scheduler_late`.
Capacity and actor conflicts become `unsent: capacity` or `actor_busy`. The
schedule never slides to accommodate slow responses. A sequence child's
scheduled timestamp is the original sequence offer, so its start lag and
scheduled-to-terminal time include dependency wait.

After the arrival window, in-flight requests and already admitted sequence
continuations may finish within `drain_timeout_ms`. At that bound, active
attempts are cancelled and remaining children are unsent. Ctrl-C stops new
arrivals and uses the same bounded drain; its report is marked interrupted.
Cancellation of a client request does not roll back a server-side write.

Plans are capped at 1 MiB, 128 actors/requests, 32 groups, 16 phases, 64 setup
steps, 64 children per sequence, 512 prepared actor/request pairs, 512 metric
series, and 1,000,000 offered child requests. Global capacity is at most 256;
group capacity cannot exceed its actor count. A phase is at most one hour,
total duration at most 24 hours, and rates at most 10000 arrivals/second/group.
Timeout/drain limits are 1–300000 ms; start-lag allowance is at most 60000 ms.
These accepted bounds are not verified service or generator capacity claims.
Responses are capped at 1 MiB; oversized responses fail and close without
unbounded draining. Other responses drain through completion or timeout.

## Reports and thresholds

Workload reports use `schema_version: 2`, `kind: workload`, and
`workload_version: 1`. Check these fields before parsing; other report kinds
retain their existing schemas. Reports omit target paths/URLs, secret binding
names/values, headers, bodies, and third-party error details. They retain authored
operation/actor/group/phase IDs, outcomes, status codes, and timing evidence.

`completed` means a terminal measured attempt, including timeout, transport,
status, or schema failure; it is not a count of successful HTTP responses.
`complete_http_responses` counts only fully consumed responses, even when status
or schema checks fail. `cancelled` means a started attempt aborted at the drain
bound. `unsent` means an offered child never started. Final request identities:

```text
offered = started + unsent
started = completed + cancelled + in_flight
completed = passed + failed
in_flight = 0 after a finished bounded drain
```

The same lifecycle counters appear per `(request, group, phase)` series and in
aggregate; `sequence_counts` counts sequence instances rather than children.
Latency samples include all completed attempts, including failures, and exclude
unsent/cancelled attempts. `latency` is actual start to terminal outcome;
`start_lag` is scheduled offer to actual start; `scheduled_to_terminal` includes
both. Each reports samples, approximate HDR p50/p95/p99 milliseconds and the
maximum observed duration; empty distributions contain null values. Rates use
the planned arrival window for offered/started and elapsed time including drain
for completed attempts. They are not all the same throughput denominator.

Threshold metrics are `p95_ms`, `max_ms`, `start_lag_p95_ms`,
`scheduled_p95_ms`, `error_rate`, and `unsent_rate`. Each has a nonnegative
`maximum`, `min_samples` (default 1), and optional `request`, `group`, `phase`
filters. Omitted filters mean all matching series; multiple filters intersect.
Equality passes. Too few samples fail with `insufficient_samples`; empty
measurements do not silently pass. `error_rate` is failed/completed and
`unsent_rate` is unsent/offered; rates range from 0 to 1.

By default any failed, unsent, cancelled, interrupted, or setup-failed run
fails. A matching `error_rate` or `unsent_rate` threshold explicitly allows
its configured maximum for the series covered by its scope. Uncovered series
remain strict; all overlapping thresholds must pass. Latency-only thresholds
do not allow failures. At least one attempt must start, and cancellation or
interruption cannot be allowed by a rate threshold.

The `run_id` identifies a report. `wall_anchor_unix_ms` associates measured
offset zero with wall time for external correlation; durations and evidence
offsets use the monotonic clock and do not depend on wall-clock adjustments.
Evidence records contain request/sequence IDs, actor, operation, group, phase,
step/replay relation, scheduled/started/completed millisecond offsets, and safe
outcomes. Unsent records have null start/completion offsets.

Capture is bounded by `evidence.max_records` and `max_bytes` (maximum 100000
records and 64 MiB); either can be zero to disable retention. The byte budget
counts compact encoded records plus one delimiter byte each, **excluding** the
report envelope; it is not a maximum report-file size. `captured_records` and
`dropped_records` describe retained versus omitted records. Dropped evidence
does not change aggregate counters or alone make execution fail.
`retention: prefix_until_first_limit` means retention stops at the first record
or byte limit, even if a later smaller record would fit. This prefix follows
record emission order, including completion and unsent bookkeeping; it is not
guaranteed to follow scheduled arrival order. Remaining offers may be accounted
analytically after drain, with omitted records included in `dropped_records`.
`evidence.complete` is false after any record drop or interrupted/cancelled
capture. An interrupted run also sets top-level `interrupted: true`; even a
fully retained prefix is not a complete uninterrupted run. Abrupt termination
or failure to publish the report can still prevent an artifact from being saved.
