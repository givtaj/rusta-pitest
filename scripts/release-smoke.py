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
    workload_writes = []
    lock = threading.Lock()
    api_token = "rusta-smoke-auth-secret"
    fixture_id = "rusta-smoke-fixture-secret"
    reserved_fixture_id = "rusta-smoke;reserved,secret"
    reserved_fixture_path = "/api/v1/widgets/rusta-smoke%3Breserved%2Csecret"
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
            elif self.path == "/workload-write":
                with lock:
                    workload_writes.append((body, self.headers.get("Idempotency-Key")))
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
                elif self.path == reserved_fixture_path and self.command == "GET":
                    payload = json.dumps({"id": reserved_fixture_id, "name": "Synthetic widget"}).encode()
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
            for partial in (" ", "\t", "y", "yes"):
                cancelled = run(["wizard"], stdin=f"3\n{origin}/wizard-must-not-run\n\n\n\n{partial}")
                assert "Cancelled." in cancelled.stdout, "partial EOF must cancel final confirmation"
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

            # Explicit byte budgets must be recognized and enforced by this
            # executable, not merely by the source-level integration tests.
            run(["openapi", "generate", "contract.json", "--out", "too-small-input.json",
                 "--max-input-bytes", "1"], expected=1)
            run(["openapi", "generate", "contract.json", "--out", "too-small-suite.json",
                 "--max-suite-bytes", "1"], expected=1)
            assert not (directory / "too-small-input.json").exists()
            assert not (directory / "too-small-suite.json").exists()
            run(["openapi", "generate", "contract.json", "--out", "budget-suite.json",
                 "--max-input-bytes", "8388608", "--max-suite-bytes", "16777216"])
            run(["suite", "run", "budget-suite.json", "--max-input-bytes", "1"], expected=1)
            run(["suite", "run", "budget-suite.json", "--max-input-bytes", "16777216"])
            assert len(requests) == before, "local byte-budget checks must send no traffic"

            # Keep the encoded fixture small while making decoded alias bytes
            # exceed its configured budget. The rejection must precede output.
            alias_marker = "rusta-smoke-decoded-value" * 32
            alias_source = ((directory / "contract.yaml").read_text()
                            + f"x-value: &value '{alias_marker}'\nx-copies:\n"
                            + "  - *value\n" * 16)
            (directory / "aliases.yaml").write_text(alias_source)
            rejected = run(["openapi", "generate", "aliases.yaml",
                            "--out", "aliases-rejected.json", "--max-input-bytes",
                            str(len(alias_source.encode()))], expected=1)
            assert "decoded string and key byte limit" in rejected.stderr
            assert "rusta-smoke-decoded-value" not in rejected.stdout + rejected.stderr
            assert not (directory / "aliases-rejected.json").exists()
            run(["openapi", "generate", "aliases.yaml", "--out", "aliases-accepted.json",
                 "--max-input-bytes", "32768"])
            aliases = json.loads((directory / "aliases-accepted.json").read_text())
            assert aliases["contract"]["x-copies"] == [alias_marker] * 16
            run(["suite", "run", "aliases-accepted.json"])
            assert len(requests) == before, "decoded-budget checks must send no traffic"

            def expanded_definitions(prefix, keyword="allOf"):
                definitions = {"n0": {"type": "object"}}
                for level in range(1, 13):
                    reference = {"$ref": f"{prefix}/n{level - 1}"}
                    children = ([reference, reference] if keyword == "allOf"
                                else {"left": reference, "right": reference})
                    definitions[f"n{level}"] = {keyword: children}
                return definitions

            # A compact shared-reference graph must become a visible blocker,
            # and execution must refuse it before making an API request.
            expanded_contract = {
                "openapi": "3.1.0", "info": {"title": "Schema budget smoke", "version": "1"},
                "paths": {"/must-not-run": {"get": {
                    "operationId": "expandedSchema", "responses": {"200": {
                        "description": "Synthetic object", "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/n12"},
                        }},
                    }},
                }}},
                "components": {"schemas": expanded_definitions("#/components/schemas")},
            }
            (directory / "expanded-contract.json").write_text(json.dumps(expanded_contract))
            run(["openapi", "generate", "expanded-contract.json", "--out", "expanded-suite.json"])
            expanded_suite = json.loads((directory / "expanded-suite.json").read_text())
            assert any("schema" in reason for reason in expanded_suite["cases"][0]["blockers"])
            run(["suite", "run", "expanded-suite.json", "--base-url", origin,
                 "--operation", "expandedSchema", "--execute"], expected=1)
            assert len(requests) == before, "expanded OpenAPI schemas must fail before traffic"

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
                for secret in (api_token, fixture_id, reserved_fixture_id, "rusta-smoke-response-secret"):
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
            reserved = suite_report("suite-reserved-fixture", [*target,
                "--operation", "getWidget", "--fixture", f"getWidget.path.id={reserved_fixture_id}",
                "--execute"], environment=environment)
            assert reserved["passed"] is True
            assert requests[-1][:2] == ("GET", reserved_fixture_path)
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
            # One offered request per group/phase keeps counts exact without a timing
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
                "phases": [{"name": name, "duration_ms": 250,
                            "rates": {"allowed": 1, "denied": 1}}
                           for name in ("first", "second")],
                "max_in_flight": 2, "timeout_ms": 2000,
                "drain_timeout_ms": 2000, "max_start_lag_ms": 500,
                "thresholds": [{"metric": "error_rate", "maximum": 0},
                               {"metric": "unsent_rate", "maximum": 0}],
                "evidence": {"max_records": 8, "max_bytes": 4096},
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
            assert preview["planned_requests"] == 4
            check_identity(preview["runner"])
            assert len(requests) == before, "workload preview must send no traffic"

            workload_environment = {"RUSTA_SMOKE_WORKLOAD_COOKIE": workload_cookie,
                                    "RUSTA_SMOKE_WORKLOAD_CSRF": workload_csrf}

            # Check expanded references and cached nonproductive cycles through
            # preview and execution, with setup present to catch early traffic.
            unsupported_schemas = {
                keyword: {"$defs": expanded_definitions("#/$defs", keyword),
                          "$ref": "#/$defs/n12"}
                for keyword in ("allOf", "dependencies")
            }
            unsupported_schemas["cached-cycle"] = {
                "$defs": {
                    "A": {"properties": {"next": {"$ref": "#/$defs/B"}},
                          "allOf": [{"$ref": "#/$defs/B"}]},
                    "B": {"$ref": "#/$defs/A"},
                },
                "$ref": "#/$defs/A",
            }
            for name, schema in unsupported_schemas.items():
                expanded_workload = json.loads(json.dumps(workload))
                expanded_workload["requests"]["deniedRead"]["response_schema"] = schema
                expanded_workload["setup"] = [{"actor": "anonymous", "request": "deniedRead"}]
                filename = f"unsupported-{name}.plan.json"
                (directory / filename).write_text(json.dumps(expanded_workload))
                for execute in (False, True):
                    output = f"unsupported-{name}-{execute}.report.json"
                    arguments = ["workload", "run", filename, "--base-url", origin,
                                 "--operation", "allowedRead", "--operation", "deniedRead",
                                 "--output", output]
                    if execute:
                        arguments.append("--execute")
                    rejected = run(arguments, expected=1, environment=workload_environment)
                    assert "response_schema_unsupported" in rejected.stderr, rejected.stderr
                    assert not (directory / output).exists()
                    assert len(requests) == before, "unsupported schemas must fail before setup or load"

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
                assert counts[key] == 4, (key, counts)
            for key in ("failed", "cancelled", "unsent", "in_flight"):
                assert counts[key] == 0, (key, counts)
            assert workload_report["metrics"]["latency"]["samples"] == 4
            assert all(row["passed"] for row in workload_report["thresholds"])
            assert {row["request"] for row in workload_report["series"]} == {"allowedRead", "deniedRead"}
            assert {row["phase"] for row in workload_report["series"]} == {"first", "second"}
            assert all(row["metrics"]["counts"]["passed"] == 1
                       for row in workload_report["series"])
            evidence = workload_report["evidence"]
            assert evidence["complete"] is True and evidence["captured_records"] == 4
            assert evidence["dropped_records"] == 0
            assert {record["status"] for record in evidence["records"]} == {200, 401}
            assert len(requests) == before + 4
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

            def workload_case(name, plan, expected=0, extra=(), environment=None):
                filename = f"{name}.plan.json"
                (directory / filename).write_text(json.dumps(plan))
                arguments = ["workload", "run", filename, "--base-url", origin,
                             "--execute", "--output", f"{name}.report.json"]
                for operation in plan["requests"]:
                    arguments.extend(["--operation", operation])
                result = run([*arguments, *extra], expected, environment=environment)
                document = json.loads((directory / f"{name}.report.json").read_text())
                assert document["schema_version"] == 2 and document["kind"] == "workload"
                assert document["passed"] == (expected == 0)
                check_identity(document["runner"])
                counts = document["counts"]
                assert counts["offered"] == counts["started"] + counts["unsent"]
                assert counts["started"] == counts["completed"] + counts["cancelled"]
                assert counts["completed"] == counts["passed"] + counts["failed"]
                assert counts["in_flight"] == 0
                return result, document

            write_key = "rusta-smoke-immutable-idempotency-secret"
            write_body = b"immutable\x00rusta-smoke-body-secret\xff\n"
            write_plan = {
                "workload_version": 1, "actors": {"writer": {}},
                "requests": {"write": {
                    "method": "POST", "path": "/workload-write",
                    "body_file": "runtime-body-secret.bin",
                    "content_type": "application/octet-stream",
                    "idempotency_key_env": "RUSTA_SMOKE_WRITE_KEY", "expect_status": 200,
                }},
                "groups": {"writes": {"actors": ["writer"], "max_in_flight": 1,
                    "sequence": [{"request": "write"}, {"request": "write", "replay_of": 0}]}},
                "phases": [{"name": "replay", "duration_ms": 100, "rates": {"writes": 1}}],
                "max_in_flight": 1, "max_start_lag_ms": 500,
                "timeout_ms": 2000, "drain_timeout_ms": 2000,
            }
            (directory / "write-preview.plan.json").write_text(json.dumps(write_plan))
            before = len(requests)
            preview = json.loads(run(["workload", "run", "write-preview.plan.json"]).stdout)
            assert preview["requires_write_opt_in"] is True
            assert preview["runtime_secrets_loaded"] is False
            run(["workload", "run", "write-preview.plan.json", "--execute",
                 "--base-url", origin, "--operation", "write"], expected=1)
            assert len(requests) == before, "unapproved writes must fail before traffic"
            (directory / "runtime-body-secret.bin").write_bytes(write_body)
            result, replay = workload_case("workload-replay", write_plan,
                extra=("--allow-writes",), environment={"RUSTA_SMOKE_WRITE_KEY": write_key})
            assert replay["counts"]["offered"] == replay["counts"]["passed"] == 2
            assert replay["sequence_counts"]["offered"] == replay["sequence_counts"]["passed"] == 1
            assert workload_writes == [(write_body, write_key), (write_body, write_key)]
            for secret in (write_key, "rusta-smoke-body-secret", "runtime-body-secret.bin",
                           "RUSTA_SMOKE_WRITE_KEY", "/workload-write"):
                assert secret not in json.dumps(replay)
                assert secret not in result.stdout and secret not in result.stderr

            failed_threshold = json.loads(json.dumps(workload))
            failed_threshold["phases"] = [{"name": "threshold", "duration_ms": 100,
                                           "rates": {"allowed": 1, "denied": 1}}]
            # A zero maximum fails for every positive measured duration; this
            # checks the exit contract without asserting a host speed target.
            failed_threshold["thresholds"] = [{"metric": "max_ms", "maximum": 0}]
            _, failed = workload_case("workload-threshold-failure", failed_threshold,
                                      expected=1, environment=workload_environment)
            assert failed["counts"]["failed"] == 0
            assert failed["thresholds"][0]["reason"] == "maximum_exceeded"

            bounded_capture = json.loads(json.dumps(failed_threshold))
            bounded_capture["thresholds"] = [{"metric": "error_rate", "maximum": 0},
                                             {"metric": "unsent_rate", "maximum": 0}]
            bounded_capture["evidence"] = {"max_records": 1, "max_bytes": 4096}
            _, bounded = workload_case("workload-capture-limit", bounded_capture,
                                       environment=workload_environment)
            assert bounded["counts"]["offered"] == bounded["counts"]["passed"] == 2
            assert bounded["evidence"]["captured_records"] == 1
            assert bounded["evidence"]["dropped_records"] == 1
            assert bounded["evidence"]["complete"] is False
            assert bounded["evidence"]["retention"] == "prefix_until_first_limit"
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)
    print(f"Release smoke passed: {version}; GET, POST, duration, statuses, timeout, JSON accounting, OpenAPI JSON/YAML/URL generation and byte limits, decoded alias budgets, expanded schema preflight, previews, authenticated GET/HEAD, reserved path data, partial-EOF cancellation, schema failure, workload actors/phases/denial/accounting, write opt-in/replay, thresholds/capture loss, redaction and output preservation")


if __name__ == "__main__":
    main()
