# Alpha release policy

This policy defines the minimum evidence required to publish a rusta-pitest
alpha. A failed required threshold blocks publication; a historical pass or a
pass from another revision does not substitute for the release candidate.

## Required thresholds

| Area | Threshold | Evidence |
| --- | --- | --- |
| Source identity | One committed release revision; package version and tag match exactly; no source changes after validation | Candidate SHA, `Cargo.toml`, lockfile, and tag |
| Formatting and lint | Zero formatting differences and zero Clippy warnings | `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` |
| Rust correctness | Zero failing or ignored Rust tests on each supported release platform | `cargo test --locked` on Linux x86_64 and macOS arm64 |
| Build provenance and notices | Zero failures in the Python regression suite run from a Git checkout | `python3 -m unittest discover -s tests -p 'test_*.py'` |
| Dependency audit | Zero known vulnerabilities and zero denied warnings in the locked dependency graph | Install `cargo-audit` 0.22.2 with `--locked`; run `cargo audit --deny warnings` against the current advisory database |
| Release review | Zero unresolved critical or high severity correctness or security findings | Independent review of candidate code and outgoing artifacts |
| Executable behavior | Every required smoke case passes against temporary loopback servers | Packaged binary smoke results on each supported platform |
| Artifact identity | Version equals the release version; JSON build revision equals the candidate; clean source metadata | `--version` and smoke JSON from release builds |
| Distribution integrity | Every distributed archive has a matching SHA-256 digest; extracted binary passes smoke checks | Release archives, `SHA256SUMS`, and workflow results |
| Dependency notices | Every target's locked dependency notice bundle is generated and verified, including the toolchain standard library | `scripts/generate-notices.py`, its `--check` mode, and archive contents |
| User documentation | Install instructions, supported platforms, exit codes, limitations, and release notes match the candidate | Reviewed README and release notes |

Required behavioral coverage includes successful GET and JSON POST requests,
explicit expected error statuses, unexpected status failure, timeout and
transport/body failure accounting, request-count and duration runs, concurrency,
wizard cancellation, SleepAPI dry-run and contracts, and JSON output that
preserves existing files. Tests must establish that attempted requests equal the
sum of success, unexpected status, timeout, and transport/body failure counts.
Every attempt contributes a latency sample. Smoke tests exercise the distributed
executable's version, GET/POST, failure exit codes, and saved report behavior;
the broader regression suite covers the remaining cases. From alpha.3 onward,
both built and extracted release binaries must also pass OpenAPI generation,
traffic-free preview, explicitly bound authenticated GET, response-validation
failure, write refusal, and existing-output protection smoke checks.

OpenAPI acceptance also covers API keys, security alternatives and combined
requirements, authentication/header conflicts, request budgets, rate pacing,
full-response timeouts, and response-size limits against disposable loopback
fixtures. These checks do not authorize traffic to external services.

From alpha.4 onward, built and extracted binaries must additionally pass authored
workload preview without secrets/traffic, isolated actor headers, expected denial,
schema-v2 accounting/redaction, explicit write refusal and opted-in immutable replay,
threshold-failure and bounded-evidence-loss smokes. Artifact checks also exercise
OpenAPI byte-limit options, reserved-character path encoding, and incomplete-line
EOF cancellation. Full regressions cover fixed arrivals under slowdown, reserved
group capacity, phase/scoped-threshold attribution, interrupted drain, and bounded
large-plan bookkeeping. Distribution checks verify included docs/examples and
preview the extracted workload example. Functional fixture completion must be
bounded, including teardown after a client rejects a response early.

From alpha.5 onward, built and extracted binaries must also reject decoded YAML
alias expansion beyond the configured input budget and excessive schema reference
expansion before request execution. Small supported inputs must remain usable;
rejected inputs must preserve existing output files and send no API traffic.

The initial public alpha has 46 Rust tests. The Python suite includes four
offline build-provenance regressions and notice-generation tests. The final
published release records the observed counts; all discovered tests must pass.
The provenance tests cover a
clean or modified checkout, a linked worktree, a standalone source archive, and
an archive nested inside an unrelated Git checkout. Source-archive builds must
report unknown Git identity, rather than borrowing the enclosing repository's
revision. A test-count change requires review; a count alone is not evidence
that the behavioral requirements passed.

The dependency audit runs with `cargo-audit` 0.22.2 and a freshly fetched
advisory database. Vulnerabilities or warnings denied by `--deny warnings` block
publication. No advisory suppression is part of the initial public alpha's
acceptance criteria. An advisory scan
reflects the database checked at release time, not a guarantee against future
disclosures.

Critical/high findings are those that can expose secrets, corrupt user files,
produce materially incorrect test results, or prevent a documented core workflow
from working. Lower severity limitations may ship only when recorded in the
release notes with a clear scope. Release review is not a claim of a complete
security audit.

## Performance interpretation

There is no universal requests-per-second or latency threshold for an HTTP test
runner. Results depend on the tested API, host, network, concurrency, payload,
and response size. This alpha gates on correct accounting and bounded test
completion, not a speed claim. Authored workloads provide configurable latency,
start-lag, scheduled-to-terminal, error-rate and unsent-rate thresholds with
explicit scope/sample requirements. These are caller-defined acceptance criteria,
not universal release performance targets. Direct benchmarks and generated suites
retain their documented pass rules; configurable throughput thresholds are absent.

Start at concurrency 1–4 and increase cautiously while observing the client and
target service. No maximum safe concurrency or capacity has been established;
the accepted argument range is not a verified operating limit.

Functional fixtures use controlled local responses and timeouts. A fixture pass
does not establish the capacity or current behavior of Roots, SleepAPI, or any
other external service.

## Supported release scope

- Linux x86_64 built and tested on Ubuntu 24.04. Older glibc distributions,
  musl/Alpine, and other Linux architectures are unverified.
- macOS arm64 built and tested on the selected GitHub runner. Intel macOS and
  Windows are unverified. The macOS executable is unsigned and not notarized.
- Direct benchmarks use HTTP/HTTPS over HTTP/1.1 with closed-loop concurrency
  and connection reuse. Custom headers and session cookies are not supported by
  those commands. OpenAPI suites support documented scalar headers, bearer and
  header API-key authentication, with explicitly selected GET/HEAD execution.
  Neither mode enables proxies, automatic retries, redirects, warmup, or
  distributed load generation.
- Authored workloads support static per-actor cookie/custom-header bindings,
  fixed arrivals, reserved groups, phases, explicit setup and request sequences,
  and selected opted-in POST/PUT/PATCH/DELETE operations. Bodies and replay keys
  are immutable; managed login/renewal and target lifecycle/cleanup are outside
  scope. Proxies, redirects, hidden retries and unbounded queues remain disabled.
- Request/profile/suite reports retain JSON schema version 1. Authored workload
  reports use schema version 2 and workload version 1; consumers must check both
  schema version and kind. All remain alpha interfaces. Future incompatible
  changes must be called out and use an appropriate schema version.

## Release procedure

1. Review the implementation branch and prepare a release candidate with updated
   package version, lockfile, changelog, and versioned release notes. Inspect
   outgoing files for credentials, private data, and machine-specific state.
2. Run the required checks and independent review. Resolve blocking findings and
   rerun the affected checks; any source change requires validating the final
   revision again before tagging.
3. Integrate the reviewed candidate through a pull request after required CI
   passes. The initial public snapshot may initialize `main` directly, with the
   same mandatory review and CI gates before any release. Confirm the final
   commit and validate its exact source.
4. Create the version tag at that commit. Build archives from the tag and verify
   the extracted binaries on every supported release platform. Generate and
   verify SHA-256 checksums. Generate each target's dependency notices and include
   `THIRD-PARTY-NOTICES.txt`, `THIRD-PARTY-MANIFEST.json`, and
   `RUST-STDLIB-LICENSE.html` in the archive and its internal checksum list.
   Include the tracked docs/examples with their relative paths and verify every
   payload file recursively against internal checksums after extraction.
5. Publish a GitHub prerelease only when all required thresholds pass. Attach
   platform archives and `SHA256SUMS`; record the commit, successful workflow
   links, supported platforms, and known limitations in the release notes.
6. Read back the published tag, prerelease state, and assets. Download the assets
   and verify their checksums. Record the publication result.

The public repository contains reusable source, tests, and documentation.
Machine state, private project evidence, credentials, and operational transcripts
do not belong in its history. The Cargo package uses an explicit source allowlist;
GitHub's autogenerated source archives contain all tracked files. crates.io
publication is a separate distribution decision.
Builds made from source archives without their own Git metadata report
`git_revision: null` and `source_dirty: null`. Official binary archives are built
from the tagged checkout and must carry the exact release revision and a clean
source marker.

Never move a published version tag or silently replace a release binary. If a
release has a blocking defect, mark its release notes as withdrawn, direct users
to a corrected alpha, and publish that correction under a new version.
