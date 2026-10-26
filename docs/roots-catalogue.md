# Roots request catalogue

The optional Roots catalogue provides nine named GET requests with expected
status codes. It is a convenience for a compatible API; Roots is not required
for custom requests, and selecting a target does not launch or modify services.

## Choose and run a target

```sh
# List targets locally; this sends no requests. `catalog` is an alias.
rusta-pitest catalogue

# One request with the preset's expected status.
rusta-pitest target roots-health
rusta-pitest target roots-strains-signed-out

# Select your running service's origin.
rusta-pitest target roots-hello --base-url http://127.0.0.1:5301 \
  --requests 20 --concurrency 2 --timeout-ms 5000
```

The default origin is `http://127.0.0.1:4301`. `--base-url` accepts an HTTP/HTTPS
origin with no path, query, fragment, or embedded credentials; the preset supplies
the path and query. Check the port and contract of your own running service.

`target` accepts `--requests`/`-n`, `--duration-secs`, `--concurrency`/`-c`,
`--timeout-ms`, `--expect-status`, and `--output`. An explicit status overrides the
preset. Accepting a 404 instead of 401 can make a command pass but does not prove
that the protected route exists.

In the wizard, options 1 and 2 are greeting/health shortcuts. Choose
**5 — Roots catalogue**, select **1–9 or a target ID**, then enter the base origin
and review the workload. Cancellation sends no requests.

## Available presets

Every preset sends GET without session cookies or custom headers.

| No. | ID | Path and query | Expected status |
| --- | --- | --- | --- |
| 1 | `roots-hello` | `/api/hello` | 200 |
| 2 | `roots-health` | `/api/health` | 200 |
| 3 | `roots-openapi` | `/api/openapi.json` | 200 |
| 4 | `roots-docs` | `/api/docs/` | 200 |
| 5 | `roots-session-signed-out` | `/api/auth/me` | 401 |
| 6 | `roots-customer-signed-out` | `/api/customers/me` | 401 |
| 7 | `roots-strains-signed-out` | `/api/strains?limit=1` | 401 |
| 8 | `roots-experiences-signed-out` | `/api/experiences?limit=1` | 401 |
| 9 | `roots-calendar-signed-out` | `/api/experiences/calendar?month=2026-09` | 401 |

The calendar query uses a fixed valid month for an anonymous-access check; it
is not a request for a particular user's records. The documentation path has a
trailing slash because this runner does not follow redirects.

## What a pass means

A pass means each response completed with the expected HTTP status. Generic
catalogue targets drain response bodies but do not assert their contents.
The signed-out presets check anonymous rejection, not successful authenticated
reads. A 200 health response has whatever meaning the target service assigns it;
the runner does not independently inspect a database or process.

Routes and status contracts can differ between service versions. The catalogue
is not proof that a server is running, that its deployment is current, or that
its access controls are complete. Use custom GET/POST for other endpoints.
Catalogue commands do not implement authentication, custom headers, cookies,
or PATCH. Separate [OpenAPI suites](openapi-suites.md) support bearer/header
API-key bindings for explicitly selected contract GET/HEAD operations.

The runner's integration tests use local fixtures to verify catalogue selection,
URL construction, status defaults, overrides, and wizard behavior. They do not
require a Roots checkout or a live Roots service.
