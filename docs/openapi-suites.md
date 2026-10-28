# OpenAPI suites

Introduced in `v0.1.0-alpha.3`, this experimental feature generates reviewable
contract suites and executes selected GET/HEAD cases. The existing SleepAPI
profiles and benchmark report schema v1 remain supported.

## Generate and review

```sh
cargo run -- openapi generate examples/widgets.openapi.yaml --out /tmp/widgets.suite.json
cargo run -- suite run /tmp/widgets.suite.json
```

`openapi generate` accepts a local filesystem path or an HTTP(S) URL to raw
OpenAPI JSON/YAML. For example:

```sh
cargo run -- openapi generate https://docs.example.test/openapi.json --out /tmp/api.suite.json
```

Supply the raw contract, rather than a Swagger UI HTML page. Only the supplied
document is downloaded; generation does not call its operations, retrieve
references or external examples, acquire tokens, or execute contract content.
Contract downloads use a five-second full-body timeout with redirects, retries,
and environment proxies disabled. URL user information and fragments are
rejected. For a private contract, download it separately and supply its local
path. Source URLs, including query values, are not stored in source metadata.

The generated JSON has `suite_version: 1`, a frozen `contract`, the original
source byte hash and OpenAPI version in `source`, and a deterministic `cases`
array. Every recognized operation remains present, including blocked writes.
The suite includes imported documentation and examples in its frozen contract;
review that content before sharing the file. Runtime credentials never enter
the generated suite. Generation does not copy credential examples into requests.

The importer supports OpenAPI 3.1.0, 3.1.1, and 3.1.2. Other versions receive an
explicit error. These are supported-subset structural and semantic checks, not
a claim to implement every OpenAPI validation rule. JSON/YAML duplicate keys,
multiple documents, non-JSON-compatible YAML, and excessive nesting are rejected.
Published alpha.3 limits contracts to 1 MiB and generated/input suites to 4 MiB.
The development branch adds explicit byte-limit options:

| Command option | Default | Accepted range |
| --- | --- | --- |
| `openapi generate --max-input-bytes` | 1048576 (1 MiB) | 1–67108864 bytes |
| `openapi generate --max-suite-bytes` | 4194304 (4 MiB) | 1–67108864 bytes |
| `suite run --max-input-bytes` | 4194304 (4 MiB) | 1–67108864 bytes |

These are byte budgets, not disabled validation. Structural limits are unchanged:
256 operations, 128 parameters per list, bounded nodes/nesting, local references,
and the supported serializer/schema subset still apply. A larger generated suite
may also need a matching explicit input limit when previewing or executing it:

```sh
cargo run --locked -- openapi generate examples/widgets.openapi.yaml \
  --max-input-bytes 8388608 --max-suite-bytes 16777216 \
  --out /tmp/widgets-large-budget.suite.json
cargo run --locked -- suite run /tmp/widgets-large-budget.suite.json \
  --max-input-bytes 16777216
```

## Bind an environment and execute

Preview is the default. It makes no operation requests and does not read secret
environment variable values. It shows per-case reasons such as missing target,
fixture, authentication binding, or unsupported semantics. Preview success means
the plan was produced; its report has `passed: null`, not an execution pass.

To execute the example, first prepare a synthetic local API with a known widget
and a test token in `TEST_API_TOKEN`. Then explicitly select its GET operation:

```sh
cargo run -- suite run /tmp/widgets.suite.json \
  --base-url http://127.0.0.1:8080/api/v1 \
  --operation getWidget \
  --fixture getWidget.path.id=w_001 \
  --auth-env bearer=TEST_API_TOKEN \
  --execute --output /tmp/widgets.report.json
```

This sends one GET to `/api/v1/widgets/w_001`. The POST remains unexecuted.
Contract `servers` do not silently choose a target. A configured base path is
preserved; credentials, queries and fragments are not permitted in the target
base URL. Fixture values are encoded as parameter data. Dot-segment paths and
unsupported path spellings are refused.

Repeat `--operation`, `--fixture`, and `--auth-env` for multiple selections or
bindings. Bindings use `NAME=VALUE`, and authentication values name environment
variables. Required path parameters always need fixture slots named
`OPERATION_ID.path.PARAMETER_NAME`; a schema-valid example is not proof that a
resource exists. Fixture strings are validated first as strings, then as JSON
primitives/arrays when the schema requires them. Other required parameters use
bounded, schema-validated synthetic candidates; optional parameters are omitted
unless a reviewed suite supplies a candidate or fixture binding.

The supported authentication schemes are HTTP bearer and API keys in headers.
Security alternatives and combined requirements are preserved. A complete
alternative must be configured; unsupported schemes do not silently become
anonymous requests. OAuth acquisition, session/cookie flows, and certificate
setup are outside this MVP. Contract-fetch credentials are never forwarded.
The separate [unreleased workload runner](workloads.md) supports authored static
actor cookie/header bindings; it does not change generated-suite authentication.

Execution checks every selected case before the first request. A missing
fixture/credential, invalid header, unsupported schema, or blocked operation
stops the entire selected run before traffic. An execution error advises using
preview for per-case details.

Each selected operation runs once, sequentially. Defaults are one request start
per second, a five-second full-response timeout, and at most 100 selected
requests. Use `--rate-per-second`, `--timeout-ms`, and `--max-requests` to change
those bounds. There is no concurrent or repeated load mode for suites. Redirects,
retries, and environment proxies are disabled; TLS verification remains enabled.
GET/HEAD still require an explicitly reviewed target and operation selection.

## Assertions and coverage

The initial request serializers support scalar path/header parameters with
`simple`, and scalar or scalar-array query parameters with `form` and either
`explode` value. Cookie parameters, object parameters, other styles,
`allowReserved: true`, and GET/HEAD bodies block the affected case. JSON request
body candidates for writes are review material only; writes cannot execute.

Response checks include:

- An explicitly documented 2xx status or `2XX` range. Documented 4xx/5xx responses
  still fail positive cases, and `default` cannot turn an unexpected success
  status into a documented positive result.
- Exact status before status-range before `default` when selecting response
  definitions for diagnostics.
- Supported JSON media types, JSON syntax, and the declared body schema. A
  missing schema is reported as `not_documented`; it gives no body-shape claim.
- Supported scalar response headers, including presence when required. Optional
  absent headers do not fail. Content-Type comes from the response content map.
- HEAD and bodyless statuses have no body-schema assertion. A 202 response
  checks only the immediate response, not eventual completion.

JSON Schema Draft 2020-12 constraints and OpenAPI 3.1 annotations are preserved,
including `$ref` siblings and productive recursive schemas. Only same-document
JSON Pointer references are supported; external references are never retrieved.
Custom dialects, `$id`, anchors, dynamic references, and non-descending reference
cycles block the case. Schema regular expressions use a bounded, linear-time
engine; unsupported expressions block schema compilation. `format`, `readOnly`,
`writeOnly`, and content annotations are not additional validation assertions.
Fixture synthesis is deliberately narrower than response validation and may
leave a case blocked when it cannot construct a valid candidate.

Business meaning, real resource IDs, operation dependencies, eventual state,
setup and cleanup cannot be inferred reliably. No cleanup or arbitrary hooks
execute. Writes in generated suites, external references, OpenAPI 3.0/3.2 adapters,
and broader property-based testing remain separate follow-on work. The unreleased
authored workload runner supports explicit write and expected-denial plans;
generated suites do not acquire those permissions automatically.

## Reports and editable suites

`--output` creates a separate report with `schema_version: 1` and `kind: suite`;
existing benchmark reports are unchanged. Reports record build identity, suite
and contract digests, execution settings, per-case outcomes/check coverage, and
total/selected/passed/failed/blocked/skipped counts. Unselected operations remain
visible. A successful selected run does not claim complete contract coverage.

Reports omit target URLs, fixture values, auth bindings and values, response
payloads, and third-party error details. Failures use reason codes rather than
echoing data. Operation identifiers come from the contract and remain visible.
The source byte hash describes ingestion provenance; the suite digest describes
the actual reviewed file. The contract digest describes the bundled contract.

Suites can be reviewed and candidate values or fixture slots edited. Before
running, the runner rebuilds operation assertions from the frozen contract and
rejects deleted operations, changed security/schema expectations, or removed
blockers. Use `--operation` to choose execution. Regenerate to change a contract.
The suite is local input, not a signed artifact or a trust boundary against its
author; review it before supplying credentials.

Both suite and report publication use exclusive creation. Existing destinations
are never overwritten, including a file created while a run is in progress.
An unusable report destination is rejected before requests begin. Exit status
is zero for successful generation/preview and successful selected execution;
invalid input, preflight failure, assertion failure, or output failure is nonzero.
