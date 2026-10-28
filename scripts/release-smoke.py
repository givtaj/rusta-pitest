#!/usr/bin/env python3
"""Exercise a release executable against disposable loopback fixtures only."""

import argparse
import json
import math
import os
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
    api_headers = []
    workload_headers = []
    lock = threading.Lock()
    api_token = "rusta-smoke-auth-secret"
    fixture_id = "rusta-smoke-fixture-secret"
    workload_cookie = "session=rusta-workload-cookie-secret"
    workload_csrf = "rusta-workload-csrf-secret"
    workload_path = "/workload-smoke/runtime-private-path"
    widget_response = {"200": {
        "description": "Synthetic widget",
        "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}},
    }}
    widget_parameter = [{"name": "id", "in": "path", "required": True,
                         "schema": {"type": "string"}}]
    openapi_contract = {
        "openapi": "3.1.0", "info": {"title": "Release smoke fixture", "version": "1"},
        "security": [{"bearer": []}],
        "paths": {
            "/widgets/{id}": {"get": {"operationId": "getWidget",
                "parameters": widget_parameter, "responses": widget_response}},
            "/broken/{id}": {"get": {"operationId": "getBroken",
                "parameters": widget_parameter, "responses": widget_response}},
            "/empty": {"head": {"operationId": "headEmpty",
                "responses": {"204": {"description": "No body"}}}},
            "/widgets": {"post": {"operationId": "createWidget",
                "responses": {"201": {"description": "Created"}}}},
        },
        "components": {
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
            "schemas": {"Widget": {"type": "object", "required": ["id", "name"],
                "properties": {"id": {"type": "string"}, "name": {"type": "string"}}}},
        },
    }

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            self.respond()

        def do_POST(self):
            self.respond()

        def do_HEAD(self):
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
            status = 503 if self.path == "/failure" else 200
            if self.path == "/openapi.json":
                payload = json.dumps(openapi_contract).encode()
            elif self.path == workload_path:
                cookie = self.headers.get("Cookie")
                csrf = self.headers.get("X-CSRF-Token")
                with lock:
                    workload_headers.append((cookie, csrf))
                authenticated = cookie == workload_cookie and csrf == workload_csrf
                status = 200 if authenticated else 401
                payload = json.dumps({
                    "ok": authenticated, "denied": not authenticated,
                    "private": "rusta-workload-response-secret",
                }).encode()
            elif self.path.startswith("/api/v1/"):
                authorization = self.headers.get("Authorization")
                with lock:
                    api_headers.append((self.path, authorization))
                if authorization != f"Bearer {api_token}":
                    status, payload = 401, b'{"error":"synthetic unauthorized"}'
                elif self.path == "/api/v1/empty" and self.command == "HEAD":
                    status, payload = 204, b""
                elif self.path == f"/api/v1/widgets/{fixture_id}" and self.command == "GET":
                    payload = json.dumps({"id": fixture_id, "name": "Synthetic widget"}).encode()
                elif self.path == f"/api/v1/broken/{fixture_id}" and self.command == "GET":
                    payload = json.dumps({"id": fixture_id, "name": 7,
                                          "private": "rusta-smoke-response-secret"}).encode()
                else:
                    status, payload = 404, b'{"error":"unexpected synthetic route"}'
            try:
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                if self.command != "HEAD":
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

            def run(arguments, expected=0, stdin="", environment=None):
                env = os.environ.copy()
                if environment:
                    env.update(environment)
                result = subprocess.run(
                    [str(binary), *arguments], cwd=directory, input=stdin,
                    capture_output=True, text=True, timeout=15, env=env,
                )
                assert result.returncode == expected, (
                    arguments, result.returncode, result.stdout, result.stderr
                )
                return result

            def check_identity(identity):
                assert identity["version"] == version
                assert identity["build_profile"] == "release"
                if args.expected_revision:
                    assert identity["git_revision"] == args.expected_revision
                if args.expected_target:
                    assert identity["target"] == args.expected_target
                if args.require_clean:
                    assert identity["source_dirty"] is False

            def report(name, arguments, expected=0, count=None):
                run([*arguments, "--output", f"{name}.json"], expected)
                document = json.loads((directory / f"{name}.json").read_text())
                assert document["schema_version"] == 1
                assert document["passed"] == (expected == 0)
                check_identity(document["runner"])
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

            # Exercise the feature using this exact executable, including when
            # the caller is testing the extracted release archive's binary.
            (directory / "contract.json").write_text(json.dumps(openapi_contract))
            (directory / "contract.yaml").write_text("""openapi: 3.1.0
info: {title: Release smoke fixture, version: "1"}
security: [{bearer: []}]
paths:
  /widgets/{id}:
    get:
      operationId: getWidget
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: Synthetic widget
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Widget"}
  /broken/{id}:
    get:
      operationId: getBroken
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: Synthetic widget
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Widget"}
  /empty:
    head:
      operationId: headEmpty
      responses: {"204": {description: No body}}
  /widgets:
    post:
      operationId: createWidget
      responses: {"201": {description: Created}}
components:
  securitySchemes:
    bearer: {type: http, scheme: bearer}
  schemas:
    Widget:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
""")
            before = len(requests)
            run(["openapi", "generate", "contract.json", "--out", "suite.json"])
            run(["openapi", "generate", "contract.yaml", "--out", "yaml-suite.json"])
            assert len(requests) == before, "local generation must not call API operations"
            suite = json.loads((directory / "suite.json").read_text())
            yaml_suite = json.loads((directory / "yaml-suite.json").read_text())
            assert suite["suite_version"] == 1
            assert suite["contract"] == yaml_suite["contract"]
            assert suite["cases"] == yaml_suite["cases"]
            assert len(suite["cases"]) == 4
            write_case = next(case for case in suite["cases"] if case["id"] == "createWidget")
            assert write_case["blockers"], "writes must remain visible and blocked"
            original_suite = (directory / "suite.json").read_bytes()
            run(["openapi", "generate", "contract.json", "--out", "suite.json"], expected=1)
            assert (directory / "suite.json").read_bytes() == original_suite
            assert len(requests) == before

            run(["openapi", "generate", f"{origin}/openapi.json", "--out", "url-suite.json"])
            assert len(requests) == before + 1
            assert requests[-1][:2] == ("GET", "/openapi.json")
            assert json.loads((directory / "url-suite.json").read_text())["cases"] == suite["cases"]

            def suite_report(name, arguments, expected=0, environment=None):
                result = run(["suite", "run", "suite.json", *arguments,
                              "--output", f"{name}.json"], expected, environment=environment)
                document = json.loads((directory / f"{name}.json").read_text())
                assert document["schema_version"] == 1 and document["kind"] == "suite"
                check_identity(document["runner"])
                for key in ("source_digest", "contract_digest", "suite_digest"):
                    assert re.fullmatch(r"[0-9a-f]{64}", document[key])
                for secret in (api_token, fixture_id, "rusta-smoke-response-secret"):
                    assert secret not in json.dumps(document), "suite report disclosed runtime data"
                    assert secret not in result.stdout and secret not in result.stderr
                return document

            before = len(requests)
            preview = suite_report("suite-unbound", [])
            assert preview["mode"] == "preview" and preview["passed"] is None
            assert preview["executed"] is False and preview["counts"]["blocked"] == 4
            target = ["--base-url", f"{origin}/api/v1", "--auth-env", "bearer=RUSTA_SMOKE_API_TOKEN"]
            get_arguments = [*target, "--operation", "getWidget",
                             "--fixture", f"getWidget.path.id={fixture_id}"]
            preview = suite_report("suite-ready", get_arguments)
            assert preview["passed"] is None and preview["ready"] is True
            assert preview["counts"]["ready"] == 1
            assert len(requests) == before, "suite previews must send no traffic"
            run(["suite", "run", "suite.json", *target, "--operation", "createWidget",
                 "--execute"], expected=1,
                environment={"RUSTA_SMOKE_API_TOKEN": api_token})
            assert len(requests) == before, "blocked write execution must send no traffic"

            environment = {"RUSTA_SMOKE_API_TOKEN": api_token}
            success = suite_report("suite-get", [*get_arguments, "--execute"], environment=environment)
            assert success["passed"] is True and success["counts"]["passed"] == 1
            assert success["counts"]["skipped"] == 3 and success["counts"]["total"] == 4
            row = next(case for case in success["cases"] if case["id"] == "getWidget")
            assert row["checks"]["body_schema"] == "passed"
            assert len(requests) == before + 1
            assert requests[-1][:2] == ("GET", f"/api/v1/widgets/{fixture_id}")
            assert api_headers[-1] == (f"/api/v1/widgets/{fixture_id}", f"Bearer {api_token}")

            head = suite_report("suite-head", [*target, "--operation", "headEmpty", "--execute"],
                                environment=environment)
            assert head["passed"] is True and head["counts"]["passed"] == 1
            row = next(case for case in head["cases"] if case["id"] == "headEmpty")
            assert row["checks"]["body_schema"] == "not_applicable"
            failure = suite_report("suite-schema-failure", [*target, "--operation", "getBroken",
                "--fixture", f"getBroken.path.id={fixture_id}", "--execute"],
                expected=1, environment=environment)
            assert failure["passed"] is False and failure["counts"]["failed"] == 1
            row = next(case for case in failure["cases"] if case["id"] == "getBroken")
            assert row["checks"]["body_schema"] == "failed"
            assert "response_schema_mismatch" in row["reasons"]

            original = (directory / "suite-get.json").read_bytes()
            before = len(requests)
            run(["suite", "run", "suite.json", *get_arguments, "--execute",
                 "--output", "suite-get.json"], expected=1, environment=environment)
            assert len(requests) == before, "existing suite report must fail before traffic"
            assert (directory / "suite-get.json").read_bytes() == original
            assert not any(method == "POST" and path.startswith("/api/")
                           for method, path, _ in requests)

            # Exercise the separate workload CLI from the packaged executable.
            # One offered request per group keeps counts exact without a timing
            # or service-capacity claim, while sharing the transport across actors.
            workload = {
                "workload_version": 1,
                "actors": {
                    "authenticated": {"headers_env": {
                        "Cookie": "RUSTA_SMOKE_WORKLOAD_COOKIE",
                        "X-CSRF-Token": "RUSTA_SMOKE_WORKLOAD_CSRF",
                    }},
                    "anonymous": {},
                },
                "requests": {
                    "allowedRead": {
                        "method": "GET", "path": workload_path, "expect_status": 200,
                        "response_schema": {"type": "object", "required": ["ok"],
                                            "properties": {"ok": {"const": True}}},
                    },
                    "deniedRead": {
                        "method": "GET", "path": workload_path, "expect_status": 401,
                        "response_schema": {"type": "object", "required": ["denied"],
                                            "properties": {"denied": {"const": True}}},
                    },
                },
                "groups": {
                    "allowed": {"actors": ["authenticated"], "max_in_flight": 1,
                                "requests": ["allowedRead"]},
                    "denied": {"actors": ["anonymous"], "max_in_flight": 1,
                               "requests": ["deniedRead"]},
                },
                "phases": [{"name": "smoke", "duration_ms": 500,
                            "rates": {"allowed": 1, "denied": 1}}],
                "max_in_flight": 2, "timeout_ms": 2000,
                "drain_timeout_ms": 2000, "max_start_lag_ms": 500,
                "thresholds": [{"metric": "error_rate", "maximum": 0},
                               {"metric": "unsent_rate", "maximum": 0}],
                "evidence": {"max_records": 4, "max_bytes": 4096},
            }
            (directory / "workload.json").write_text(json.dumps(workload))
            workload_target = ["workload", "run", "workload.json", "--base-url", origin]
            before = len(requests)
            preview_result = run([*workload_target, "--output", "workload-preview.json"],
                                 environment={"RUSTA_SMOKE_WORKLOAD_COOKIE": "",
                                              "RUSTA_SMOKE_WORKLOAD_CSRF": ""})
            preview = json.loads((directory / "workload-preview.json").read_text())
            assert json.loads(preview_result.stdout) == preview
            assert preview["schema_version"] == 2 and preview["kind"] == "workload"
            assert preview["workload_version"] == 1 and preview["mode"] == "preview"
            assert preview["executed"] is False and preview["passed"] is None
            assert preview["runtime_secrets_loaded"] is False
            assert preview["planned_requests"] == 2
            check_identity(preview["runner"])
            assert len(requests) == before, "workload preview must send no traffic"

            workload_environment = {"RUSTA_SMOKE_WORKLOAD_COOKIE": workload_cookie,
                                    "RUSTA_SMOKE_WORKLOAD_CSRF": workload_csrf}
            workload_arguments = [*workload_target, "--operation", "allowedRead",
                                  "--operation", "deniedRead", "--execute",
                                  "--output", "workload-report.json"]
            workload_result = run(workload_arguments, environment=workload_environment)
            workload_report = json.loads((directory / "workload-report.json").read_text())
            assert json.loads(workload_result.stdout) == workload_report
            assert workload_report["schema_version"] == 2
            assert workload_report["kind"] == "workload" and workload_report["workload_version"] == 1
            assert workload_report["mode"] == "execute" and workload_report["executed"] is True
            assert workload_report["passed"] is True
            check_identity(workload_report["runner"])
            counts = workload_report["counts"]
            for key in ("offered", "started", "completed", "passed", "complete_http_responses"):
                assert counts[key] == 2, (key, counts)
            for key in ("failed", "cancelled", "unsent", "in_flight"):
                assert counts[key] == 0, (key, counts)
            assert workload_report["metrics"]["latency"]["samples"] == 2
            assert all(row["passed"] for row in workload_report["thresholds"])
            assert {row["request"] for row in workload_report["series"]} == {"allowedRead", "deniedRead"}
            assert all(row["metrics"]["counts"]["passed"] == 1
                       for row in workload_report["series"])
            evidence = workload_report["evidence"]
            assert evidence["complete"] is True and evidence["captured_records"] == 2
            assert evidence["dropped_records"] == 0
            assert {record["status"] for record in evidence["records"]} == {200, 401}
            assert len(requests) == before + 2
            assert all(method == "GET" and path == workload_path
                       for method, path, _ in requests[before:])
            assert set(workload_headers) == {(workload_cookie, workload_csrf), (None, None)}, (
                "workload actors must retain separate runtime headers", workload_headers
            )
            for secret in (workload_cookie, workload_csrf, workload_path, origin,
                           "RUSTA_SMOKE_WORKLOAD_COOKIE", "RUSTA_SMOKE_WORKLOAD_CSRF",
                           "rusta-workload-response-secret"):
                for document in (preview, workload_report):
                    assert secret not in json.dumps(document), "workload report disclosed runtime data"
                for result in (preview_result, workload_result):
                    assert secret not in result.stdout and secret not in result.stderr
            original = (directory / "workload-report.json").read_bytes()
            before = len(requests)
            run(workload_arguments, expected=1, environment=workload_environment)
            assert len(requests) == before, "existing workload report must fail before traffic"
            assert (directory / "workload-report.json").read_bytes() == original
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
    print(f"Release smoke passed: {version}; GET, POST, duration, statuses, timeout, JSON accounting, OpenAPI JSON/YAML/URL generation, previews, authenticated GET/HEAD, schema failure, workload actors/expected denial/accounting, redaction and output preservation")


if __name__ == "__main__":
    main()
