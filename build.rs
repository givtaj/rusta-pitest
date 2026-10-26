use std::{env, fs, path::Path, process::Command};

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    // A source archive may be unpacked inside an unrelated checkout. Only the
    // repository rooted at this package can supply its build identity.
    let manifest_dir =
        env::var_os("CARGO_MANIFEST_DIR").and_then(|path| fs::canonicalize(path).ok());
    let repository = manifest_dir.as_ref().filter(|directory| {
        git(directory, &["rev-parse", "--show-toplevel"])
            .and_then(|root| fs::canonicalize(root).ok())
            .as_ref()
            == Some(directory)
    });
    let git_value = |args: &[&str]| repository.and_then(|directory| git(directory, args));

    for path in ["build.rs", "Cargo.toml", "Cargo.lock", "src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    // Follow the actual Git paths, including linked worktrees and detached HEADs.
    for name in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_value(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git_value(&["symbolic-ref", "--quiet", "HEAD"])
        && let Some(path) = git_value(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let revision = git_value(&["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
    let dirty = git_value(&[
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "src",
    ])
    .map(|status| (!status.is_empty()).to_string())
    .unwrap_or_default();
    let rustc = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    for (key, value) in [
        ("GIT_REVISION", revision),
        ("SOURCE_DIRTY", dirty),
        ("RUSTC_VERSION", rustc),
        ("BUILD_TARGET", env::var("TARGET").unwrap_or_default()),
        ("BUILD_PROFILE", env::var("PROFILE").unwrap_or_default()),
    ] {
        println!("cargo:rustc-env=RUSTA_{key}={value}");
    }
}
