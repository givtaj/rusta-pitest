import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

request_path = "/synthetic-capacity-demo"
cookie_name = "RUSTA_CAPACITY_TEST_COOKIE"
cookie = "session=synthetic-capacity-demo"
slots = threading.BoundedSemaphore(2)
request_limit = threading.BoundedSemaphore(80)

class CapacityService(BaseHTTPRequestHandler):
    def setup(self):
        self.request.settimeout(2)
        super().setup()

    def do_GET(self):
        if not request_limit.acquire(blocking=False):
            return self.reply(429)
        if self.path != request_path or self.headers.get("Cookie") != cookie:
            return self.reply(401)
        if not slots.acquire(blocking=False):
            time.sleep(0.04)
            return self.reply(503)
        try:
            time.sleep(0.34)
        finally:
            slots.release()
        self.reply(200)

    def reply(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass

binary, report = map(Path, sys.argv[1:])
plan = Path(__file__).with_name("plan.json")
with ThreadingHTTPServer(("127.0.0.1", 0), CapacityService) as service:
    worker = threading.Thread(target=service.serve_forever)
    worker.start()
    try:
        subprocess.run([
            binary, "workload", "capacity", plan, "--execute",
            "--base-url", f"http://127.0.0.1:{service.server_port}",
            "--operation", "read", "--output", report,
        ], env={**os.environ, cookie_name: cookie}, check=True, timeout=30,
            stdout=subprocess.DEVNULL)
    finally:
        service.shutdown()
        worker.join()

result = json.loads(report.read_text())
assert result["passed"], result
assert 1 <= result["peak_client_in_flight"] <= result["measurement"]["max_in_flight"]
for mode, first_failure, highest in (("sustained", 20, 4), ("bursts", 6, 2)):
    sequence = result[mode]
    assert sequence["first_failure"] == first_failure
    assert sequence["highest_repeatedly_passing_level"] == highest
    for trial in sequence["trials"]:
        counts = trial["counts"]
        print(mode, trial["attempt"], trial["level"], counts["by_status"],
              "peak", trial["peak_client_in_flight"],
              "success/s", round(trial["successful_requests_per_sec"], 2),
              "all p50", trial["all_attempt_latency"]["p50_ms"],
              "pass p50", trial["successful_request_latency"]["p50_ms"])
