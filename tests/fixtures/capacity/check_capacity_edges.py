#!/usr/bin/env python3
"""Exercise search and cancellation edges against a synthetic loopback service."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time


class Fixture(ThreadingHTTPServer):
    def __init__(self, failing_reads, delay_from_read):
        super().__init__(("127.0.0.1", 0), Handler)
        self.failing_reads = failing_reads
        self.delay_from_read = delay_from_read
        self.read_count = 0
        self.read_arrived = threading.Condition()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/fixture":
            self.send_error(404)
            return
        with self.server.read_arrived:
            self.server.read_count += 1
            read_number = self.server.read_count
            self.server.read_arrived.notify()
        if self.server.delay_from_read is not None and read_number >= self.server.delay_from_read:
            time.sleep(0.2)
        self.send_response(503 if read_number in self.server.failing_reads else 200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


def run_trial(binary, failing_reads, rates, *, duration_ms=120, burst_sizes=(1, 2),
              interrupt_at=None, delay_from_read=None, max_success_p95_ms=None):
    plan = {
        "capacity_version": 1,
        "operation": "read",
        "actor": {},
        "request": {"method": "GET", "path": "/fixture"},
        "warmup": {"duration_ms": 80, "rate_per_sec": 1},
        "sustained": {
            "duration_ms": duration_ms,
            "rates_per_sec": rates,
        },
        "bursts": {"sizes": burst_sizes},
        "max_in_flight": max(burst_sizes),
        "timeout_ms": 500,
        "max_start_lag_ms": 100,
    }
    if max_success_p95_ms is not None:
        plan["max_success_p95_ms"] = max_success_p95_ms
    with tempfile.TemporaryDirectory(prefix="rusta-capacity-review-") as tmp, Fixture(
        failing_reads, delay_from_read,
    ) as server:
        plan_path = Path(tmp) / "plan.json"
        plan_path.write_text(json.dumps(plan), encoding="utf-8")
        server_thread = threading.Thread(target=server.serve_forever, args=(0.05,), daemon=True)
        server_thread.start()
        command = [
            str(binary), "workload", "capacity", str(plan_path), "--execute",
            "--base-url", f"http://127.0.0.1:{server.server_port}", "--operation", "read",
        ]
        try:
            with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                try:
                    if interrupt_at is not None:
                        with server.read_arrived:
                            assert server.read_arrived.wait_for(
                                lambda: server.read_count >= interrupt_at, timeout=5,
                            )
                        process.send_signal(signal.SIGINT)
                    report_output, stderr = process.communicate(timeout=10)
                except BaseException:
                    process.kill()
                    process.communicate(timeout=3)
                    raise
                assert process.returncode == 1, (process.returncode, stderr)
            return json.loads(report_output), server.read_count
        finally:
            server.shutdown()
            server_thread.join(timeout=3)
            assert not server_thread.is_alive()


def attempts(sweep):
    return [
        {key: trial[key] for key in ("attempt", "confirmation", "level", "passed")}
        for trial in sweep["trials"]
    ]


def summarize_sweep(sweep):
    return {
        "first_failure": sweep["first_failure"],
        "highest_repeatedly_passing_level": sweep["highest_repeatedly_passing_level"],
        "configured_ceiling": sweep["configured_ceiling"],
        "attempts": attempts(sweep),
    }


def check(binary):
    report, reads = run_trial(binary, {2}, [1, 2])
    sustained = report["sustained"]
    bursts = report["bursts"]
    assert sustained["first_failure"] == 1
    assert sustained["highest_repeatedly_passing_level"] is None
    assert [trial["level"] for trial in sustained["trials"]] == [1]
    assert bursts["first_failure"] is None
    assert bursts["highest_repeatedly_passing_level"] == 2
    assert [trial["level"] for trial in bursts["trials"]] == [1, 2, 2, 2]
    assert reads == 9
    first_failure = {
        "server_reads": reads,
        "sustained": summarize_sweep(sustained),
        "bursts": summarize_sweep(bursts),
    }

    report, reads = run_trial(binary, {4, 6}, [1, 2, 9])
    sustained = report["sustained"]
    assert sustained["first_failure"] == 9
    assert sustained["highest_repeatedly_passing_level"] is None
    assert [(trial["level"], trial["passed"]) for trial in sustained["trials"]] == [
        (1, True), (2, True), (9, False), (2, False), (2, True),
    ]
    first_confirmation = {
        "server_reads": reads,
        "sustained": summarize_sweep(sustained),
    }

    report, reads = run_trial(binary, set(), [1, 2], duration_ms=1600, interrupt_at=2)
    sustained = report["sustained"]
    trial = sustained["trials"][0]
    assert report["interrupted"] and trial["interrupted"]
    assert sustained["first_failure"] is None
    assert sustained["highest_repeatedly_passing_level"] is None
    assert trial["counts"]["unsent"] == 1
    assert report["bursts"]["trials"] == [] and reads == 2
    interruption = {
        "server_reads": reads,
        "reported_unsent_offers": trial["counts"]["unsent"],
        "bursts_started": False,
        "sustained": summarize_sweep(sustained),
    }

    # One warmup and three 1/s sustained trials precede the first burst.
    report, reads = run_trial(
        binary, set(), [1], burst_sizes=(2, 3), interrupt_at=5, delay_from_read=5,
    )
    bursts = report["bursts"]
    assert report["interrupted"]
    assert report["sustained"]["highest_repeatedly_passing_level"] == 1
    assert bursts["first_failure"] is None
    assert bursts["highest_repeatedly_passing_level"] is None
    assert len(bursts["trials"]) == 1
    trial = bursts["trials"][0]
    assert trial["level"] == 2 and trial["interrupted"]
    assert trial["counts"]["by_status"] == {"200": 2}
    assert trial["counts"]["unsent"] == 0 and reads == 6
    active_burst_interruption = {
        "server_reads": reads,
        "first_burst_responses": trial["counts"]["by_status"],
        "bursts": summarize_sweep(bursts),
    }

    threshold_ms = 15
    report, reads = run_trial(
        binary, set(), [1, 2], delay_from_read=1, max_success_p95_ms=threshold_ms,
    )
    warmup = report["warmup"]
    assert warmup["passed"] and not warmup["measured"]
    assert warmup["success_p95_gate_passed"] is None
    assert warmup["successful_request_latency"]["p95_ms"] > threshold_ms
    assert reads == 3 and not report["passed"] and not report["interrupted"]
    latency_gate = {"threshold_ms": threshold_ms, "server_reads": reads}
    for name in ("sustained", "bursts"):
        sweep = report[name]
        assert sweep["first_failure"] == 1
        assert sweep["highest_repeatedly_passing_level"] is None
        assert len(sweep["trials"]) == 1
        trial = sweep["trials"][0]
        assert trial["counts"]["by_status"] == {"200": 1}
        assert trial["counts"]["passed"] == 1 and trial["counts"]["unsent"] == 0
        assert trial["success_p95_gate_passed"] is False
        assert trial["successful_request_latency"]["p95_ms"] > threshold_ms
        latency_gate[name] = {
            "successful_request_p95_ms": trial["successful_request_latency"]["p95_ms"],
            "successful_http_responses": trial["counts"]["by_status"]["200"],
            "first_failure": sweep["first_failure"],
        }
    return {
        "runner": report["runner"],
        "first_sustained_level_failed_with_independent_burst_sweep": first_failure,
        "first_confirmation_failed_and_second_still_executed": first_confirmation,
        "sigint_during_sustained_stage": interruption,
        "sigint_during_active_burst_drained_current_requests": active_burst_interruption,
        "success_p95_gate_rejects_slow_200s_in_measured_stages": latency_gate,
    }


def main():
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("binary", type=Path)
    args.add_argument("--output", type=Path)
    options = args.parse_args()
    summary = check(options.binary.resolve(strict=True))
    content = json.dumps(summary, indent=2) + "\n"
    if options.output:
        with options.output.open("x", encoding="utf-8") as output:
            output.write(content)
    print(content, end="")


if __name__ == "__main__":
    main()
