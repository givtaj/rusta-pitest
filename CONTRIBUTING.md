# Contributing

Bug reports, focused fixes, and documentation improvements are welcome. For a
large feature or a change to CLI/report semantics, open an issue before building
it so the intended behavior can be discussed.

## Get started

Install a current stable Rust toolchain with `rustfmt`, `clippy`, and `rust-docs`,
Python 3.9 or newer, and Git. Clone the public repository:

```sh
git clone https://github.com/givtaj/rusta-pitest.git
cd rusta-pitest
cargo run --locked -- --help
```

Create a branch for your change. Keep unrelated work out of the pull request and
add regression coverage when fixing behavior. Never include credentials,
private endpoints, raw customer data, or local machine configuration in a commit.

## Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
cargo build --locked --release
python3 scripts/release-smoke.py target/release/rusta-pitest
python3 scripts/generate-notices.py --output-dir target/notices
python3 scripts/generate-notices.py --output-dir target/notices --check
```

Tests use temporary loopback servers and synthetic responses. They do not require
Roots, SleepAPI, a database, or access to another project. Dependencies may need
to be downloaded on the first build. Notice generation uses locked dependency
metadata and the toolchain's standard-library copyright file. Release checks
additionally audit the lockfile with `cargo-audit` 0.22.2 and
`cargo audit --deny warnings` and verify extracted artifacts on each supported
target. See the [release policy](docs/release-policy.md).

## Pull requests and bug reports

Open a pull request against `main` describing the problem, resulting behavior,
and checks performed. Include operating system, runner version, and a minimal
synthetic reproduction in bug reports. Explain report-schema or exit-code
changes explicitly. CI must pass on Linux x86_64 and macOS arm64 before merging.

Use [GitHub issues](https://github.com/givtaj/rusta-pitest/issues) for ordinary
bugs and questions. Follow [SECURITY.md](SECURITY.md) for suspected
vulnerabilities. Be respectful and keep discussion focused on the work.

By submitting a contribution, you agree to license it under the project's
[Apache 2.0 license](LICENSE). Preserve applicable dependency notices in release
artifacts.
