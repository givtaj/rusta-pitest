# Alpha.4 CodeQL review

This review covers the 35 new CodeQL alerts on [PR #3](https://github.com/givtaj/rusta-pitest/pull/3), initially reported at
`a1549278e7e955a32fd43421775ac36ff34c0460`. The exact Rust analysis was
`2044650873`, indexed under `refs/pull/3/head`. Its SARIF contains the complete
source-to-sink paths; annotation severity alone does not establish exploitability.

## Trace inventory

The following source lines refer to that initial revision, before remediation.
All 32 path findings originate in test-only calls to `std::env::temp_dir()`.

| Rule / source | Alert numbers | Reviewed use |
| --- | --- | --- |
| Path / `src/workload.rs:1466` | 52, 53, 56, 57, 58, 59, 60, 61, 62, 63 | Unit-test plan creation, loading and cleanup |
| Path / `src/workload_http.rs:989` | 43, 44, 45, 46, 66 | Synthetic HTTP body fixtures in a test directory |
| Path / `tests/input_limits.rs:24` | 41, 42, 50, 64, 65, 68, 69 | Synthetic contract/suite files and cleanup |
| Path / `tests/workload.rs:22` | 38, 39, 40, 47, 48, 49, 51, 54, 55, 67 | Synthetic plans, bodies, reports and cleanup |
| Command / `src/workload_http.rs:1170` | 70 | Run the current test executable with one fixed test selector |
| Command / `tests/workload.rs:22` | 71 | Pass a generated plan path as a separate argument to Cargo's test executable |
| Command / `tests/input_limits.rs:24` | 72 | Pass synthetic fixture arguments to Cargo's test executable |

Alert 53 ends in production `load()` at `src/workload.rs:519`, but its full
trace starts in the unit-test `options()` helper. This is not a trace from a
network request or imported API field choosing a filesystem path. The public
CLI intentionally accepts a caller-selected workload file.

The command helpers use `std::process::Command` with separate arguments. They
do not construct shell commands. The integration-test executable is selected by
Cargo's compile-time `CARGO_BIN_EXE_rusta-pitest`, not by fixture content. The
`current_exe()` call is inside `#[cfg(test)]` and uses a fixed child test name
plus synthetic environment values. It is not present in the release executable.

## Actual defect fixed during review

The unit-test plan helper previously used `fs::write` on a predictable temporary
filename. A pre-existing file or symlink could therefore be followed and
truncated. It now creates files atomically with `create_new`, mode `0600` on
Unix, and bounded collision retries. Regressions verify that existing files and
symlink targets are preserved.

The three temporary-directory helpers now create their directories atomically
with mode `0700` on Unix. Input-limit fixture files additionally use exclusive
creation and mode `0600`. Permission and no-clobber tests also pass with
`umask 000`. Intentional fixture rewrites occur only inside the newly owned
private directory.

## Disposition boundary

After these fixes, remaining findings in the reviewed families are test-harness
uses under a trusted local test-runner environment. The appropriate disposition
is `used in tests`, with the individual alert's source, sink, mitigation and
review revision recorded in its dismissal comment. The [repository alert
history](https://github.com/givtaj/rusta-pitest/security/code-scanning) and final
PR check are authoritative for current disposition, including any new alerts
introduced by the regression tests.

This conclusion does not claim that environment-controlled temporary roots or
`current_exe()` are safe across arbitrary privilege boundaries. Tests must not
run with elevated privileges against attacker-owned executable paths or a
hostile temporary root. No production path rules, scanner queries or analysis
coverage are disabled by this review. Future findings require their own traces;
they are not automatically covered by this record.

