# Security

## Report a vulnerability privately

Use [GitHub private vulnerability reporting](https://github.com/givtaj/rusta-pitest/security/advisories/new)
for a suspected vulnerability. Do not put credentials, private response data, or
an unpatched exploit in a public issue. Include the affected version, operating
system, a minimal reproduction with synthetic data, the observed impact, and any
suggested mitigation. Avoid attaching raw benchmark reports before reviewing
URLs and other fields for sensitive data.

Ordinary bugs and usability questions belong in
[GitHub issues](https://github.com/givtaj/rusta-pitest/issues).

## Supported versions

Security fixes are made for the latest published alpha. Earlier alpha versions
may require upgrading rather than receiving a separate patch. Alpha APIs can
change, and there is no guaranteed response time or long-term support window.

## Operational scope

Run workloads only against services you own or are authorized to test. Start
with low concurrency and a small request count; repeated POSTs can mutate the
target's data. This runner does not isolate or reset the target service.

Saved request metadata omits URL credentials, query strings, fragments, and
request bodies, but URL paths remain visible. Review reports before sharing.
Command-line arguments and terminal output can also expose values to your local
environment. Direct benchmark commands have no custom-header or session-cookie
bindings. OpenAPI suites support runtime bearer/header API-key authentication.
The unreleased cookie-security extension also binds declared cookie values
through environment-variable names. It constructs one validated `Cookie`
header for each selected operation's chosen security alternative; no cookie jar,
login, or renewal is provided. Preview never reads those values, and suite
reports/diagnostics omit runtime authentication values and binding names.
Review the frozen contract before binding credentials to its operations.

The authored workload runner, introduced in alpha.4, binds complete header
values, including cookies and CSRF headers, through environment-variable names.
Actor credentials and bodies are immutable execution snapshots; the runner does
not update cookies, acquire credentials, or renew authentication automatically.
Workload reports omit target URLs/paths, binding names/values, headers, bodies,
and third-party error details, while keeping authored IDs and timing/outcome
evidence. Use non-sensitive IDs and review the plan and report before sharing.
These workload-report rules do not remove the direct-command argument/terminal
exposure described above. Environment access and target-service behavior remain
part of the caller's environment. See [workloads](docs/workloads.md).

The unreleased structured-query field stores literal names and values in the
workload plan; it does not resolve environment variables or response data.
Treat plans containing private query data as private files and review them before
sharing or committing. Query names and values are omitted from runner previews,
reports, evidence, and diagnostics, but a target or intermediary may log the
complete request URL. Do not use query parameters to transport credentials.
The runner does not control external logs, follow pagination, or redact private
data deliberately placed in visible actor/request/group/phase IDs.

The release process audits locked dependencies and verifies artifact checksums.
A passing advisory scan reflects the database checked at that time; report newly
disclosed or otherwise undetected issues through the private channel above.
