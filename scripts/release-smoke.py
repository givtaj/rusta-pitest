#!/usr/bin/env python3
"""Exercise a release executable against disposable loopback fixtures only."""

import argparse
import json
import math
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--expected-version")
    parser.add_argument("--expected-revision")
    parser.add_argument("--expected-target")
    parser.add_argument("--require-clean", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    # This repository uses a literal package version; keep the standalone
    # smoke compatible with the Python 3.9 supplied by macOS developer tools.
    manifest = (Path(__file__).resolve().parents[1] / "Cargo.toml").read_text()
    package = manifest.split("[package]", 1)[1].split("\n[", 1)[0]
    version = args.expected_version or re.search(
        r'^version\s*=\s*"([^"\n]+)"\s*$', package, re.MULTILINE
    ).group(1)
    requests = []
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            self.respond()

        def do_POST(self):
            self.respond()

        def respond(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with lock:
                requests.append((self.command, self.path, body))
            if self.path == "/slow":
                time.sleep(0.3)
            elif self.path == "/duration":
                time.sleep(0.01)
            payload = b'{"ok":true}'
            try:
                self.send_response(503 if self.path == "/failure" else 200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass  # Expected after the client's timeout smoke case.

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    origin = f"http://127.0.0.1:{server.server_port}"
    try:
        with tempfile.TemporaryDirectory(prefix="rusta-release-smoke-") as directory:
            directory = Path(directory)

            def run(arguments, expected=0, stdin=""):
                result = subprocess.run(
                    [str(binary), *arguments], cwd=directory, input=stdin,
                    capture_output=True, text=True, timeout=15,
                )
                assert result.returncode == expected, (
                    arguments, result.returncode, result.stdout, result.stderr
                )
                return result

            def report(name, arguments, expected=0, count=None):
                run([*arguments, "--output", f"{name}.json"], expected)
                document = json.loads((directory / f"{name}.json").read_text())
                assert document["schema_version"] == 1
                assert document["passed"] == (expected == 0)
                identity = document["runner"]
                assert identity["version"] == version
                assert identity["build_profile"] == "release"
                if args.expected_revision:
                    assert identity["git_revision"] == args.expected_revision
                if args.expected_target:
                    assert identity["target"] == args.expected_target
                if args.require_clean:
                    assert identity["source_dirty"] is False
                metrics = document["runs"][0]["report"]["metrics"]
                attempted = metrics["attempted"]
                assert attempted > 0
                if count is not None:
                    assert attempted == count
                assert attempted == sum(metrics[key] for key in (
                    "succeeded", "unexpected_status", "timeouts", "transport_errors"
                ))
                assert metrics["complete_responses"] == sum(metrics["status_counts"].values())
                assert metrics["latency"]["samples"] == attempted
                assert 0 < metrics["latency"]["p50_us"] <= metrics["latency"]["p95_us"] <= metrics["latency"]["p99_us"]
                elapsed = metrics["elapsed_secs"]
                assert elapsed > 0 and math.isfinite(elapsed)
                assert math.isclose(metrics["successful_responses_per_sec"], metrics["succeeded"] / elapsed)
                return document, metrics

            assert run(["--version"]).stdout.strip() == f"rusta-pitest {version}"
            run(["--help"])
            run(["catalogue"])
            run(["profile", "sleepapi", "--dry-run"])
            run(["wizard"], stdin="q\n")
            run(["get", origin, "--requests", "0"], expected=2)
            assert not requests, "preview, cancellation and invalid input must send no traffic"

            document, metrics = report("get", ["get", f"{origin}/ok?token=smoke-secret", "-n", "100", "-c", "4"], count=100)
            assert metrics["succeeded"] == 100
            assert "smoke-secret" not in json.dumps(document)
            _, metrics = report("post", ["post", f"{origin}/echo", "--json", '{"message":"alpha"}', "-n", "3", "-c", "2"], count=3)
            assert metrics["succeeded"] == 3
            posts = [json.loads(body) for method, path, body in requests if method == "POST" and path == "/echo"]
            assert posts == [{"message": "alpha"}] * 3
            _, metrics = report("status", ["get", f"{origin}/failure"], expected=1, count=1)
            assert metrics["unexpected_status"] == 1
            _, metrics = report("expected-status", ["get", f"{origin}/failure", "--expect-status", "503"], count=1)
            assert metrics["succeeded"] == 1
            _, metrics = report("timeout", ["get", f"{origin}/slow", "--timeout-ms", "75"], expected=1, count=1)
            assert metrics["timeouts"] == 1
            _, metrics = report("duration", ["get", f"{origin}/duration", "--duration-secs", "1", "-c", "2"])
            assert metrics["succeeded"] == metrics["attempted"]

            original = (directory / "get.json").read_bytes()
            before = len(requests)
            run(["get", f"{origin}/ok", "--output", "get.json"], expected=1)
            assert len(requests) == before, "existing output must fail before traffic"
            assert (directory / "get.json").read_bytes() == original
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
    print(f"Release smoke passed: {version}; GET, POST, duration, statuses, timeout, JSON accounting, redaction and output preservation")


if __name__ == "__main__":
    main()
