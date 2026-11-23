# Capacity sweep on loopback

The fixture accepts two concurrent reads. Each accepted read takes about
340 ms, and excess reads receive HTTP 503 after about 40 ms. It uses one
synthetic actor, a GET operation, and an ephemeral loopback port.

Run these commands from the repository root to generate a fresh report.
The second script checks first-level failure, failed confirmation, interruption
during sustained traffic and a burst, and the successful-request p95 limit.

```sh
cargo build --locked
binary="${CARGO_TARGET_DIR:-target}/debug/rusta-pitest"
fixture=tests/fixtures/capacity
out=$(mktemp -d)
python3 "$fixture/verify.py" "$binary" "$out/capacity.report.json"
python3 "$fixture/check_capacity_edges.py" "$binary" --output "$out/edges.json"
```

Both scripts run the normal `workload capacity` CLI. The main sweep uses the
[checked-in plan](plan.json) and prints one row per measured trial. Inspect
`warmup.measured`, then read `sustained.trials` and `bursts.trials` in order.
Each row includes the attempted level, configured ceiling, status counts,
observed client peak, successful requests per second through drain, and
latencies for successful requests and all terminal attempts.

The [sample report](capacity.report.json) and [edge check results](edges.json)
came from a macOS ARM64 debug build at source commit `bc1f301`. The sweep
excluded one warmup request. After the first sustained failure at 20 offers/s,
both confirmations at 4/s passed. After the first burst failure at size 6,
both confirmations at size 2 passed. The six-wide burst recorded two 200s,
four 503s, a 42 ms median across all attempts, and a 344 ms median for successful
reads. Timings, throughput, and observed client overlap vary by run.
