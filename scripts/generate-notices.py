#!/usr/bin/env python3
"""Generate target-specific notices from locked registry sources and rust-docs.

This deliberately covers the entire target-filtered Cargo resolution, including
build dependencies and packages that may not contribute code to the executable.
It collects supplied license/notice files and leading source attribution headers;
it does not infer SPDX permissions or claim to decide redistribution obligations.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile


REGISTRIES = {
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
}
NOTICE_NAME = re.compile(
    r"^(?:licen[cs]e|unlicense|copying|copyright|notice|authors|contributors)(?:$|[._-])",
    re.IGNORECASE,
)
ATTRIBUTION = re.compile(
    r"copyright|SPDX-License|permission is hereby|licensed under|public domain|"
    r"released under|redistribution and use", re.IGNORECASE,
)
SOURCE_SUFFIXES = {".rs", ".c", ".h", ".cc", ".cpp", ".hpp", ".s", ".asm", ".pl", ".py", ".go"}
AWS_REQUIRED = ("LICENSE", "aws-lc/LICENSE", "aws-lc/third_party/fiat/LICENSE")
AWS_COMPONENTS = (
    "BoringSSL", "OpenSSL", "mlkem-native", "mldsa-native", "Fiat Cryptography",
    "s2n-bignum", "Jitter Entropy", "Keccak", "Apache License", "ISC License",
    "MIT License", "BSD 3-Clause", "MIT No Attribution",
)

# Reviewed omissions in these exact locked releases only. The helper crates'
# original manifests inherit the workspace license, and all three archives pin
# the same repository commit. Other versions or missing-license crates still fail.
JSONSCHEMA_LICENSE_COMMIT = "4eb7a64d9cf45bcbf38c7a69e544477294460751"
JSONSCHEMA_REPOSITORY = "https://github.com/Stranger6667/jsonschema"
JSONSCHEMA_LICENSE_SHA256 = "117829c3ca21efb132d81a44b55363d395ab8eea18526873bc828da4c0e5f038"
WORKSPACE_LICENSE_PACKAGES = {
    ("jsonschema-regex", "0.60.1"): "crates/jsonschema-regex",
    ("jsonschema-value", "0.60.1"): "crates/jsonschema-value",
}

# These crate archives omit the repository's supplied license. This verbatim
# supplement was fetched from the commit recorded inside both locked archives;
# it is not synthesized from SPDX metadata or borrowed from a different project.
SIMD_LICENSE_COMMIT = "d74c030d9dc4f3cae02146d1f497ff62726ef09a"
SIMD_REPOSITORY = "https://github.com/Nugine/simd"
SIMD_LICENSE_FILE = "licenses/Nugine-simd-d74c030d-LICENSE"
SIMD_LICENSE_SHA256 = "71674605ec4c087fe9eb534e3e4f9e26eb2e4aabcd76a29fd156c6a844d44b3d"
UPSTREAM_LICENSE_PACKAGES = {
    ("uuid-simd", "0.8.0"): "crates/uuid-simd",
    ("vsimd", "0.8.0"): "crates/vsimd",
}
SUPPLEMENT_ROOT = Path(__file__).resolve().parents[1]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True) + "\n").encode()


def safe_relative(relative):
    path = PurePosixPath(relative)
    if path.is_absolute() or ".." in path.parts or "\\" in relative:
        raise ValueError(f"unsafe package-relative path: {relative!r}")
    return path


def locked_checksums(data):
    """Read Cargo's restricted lock identity fields without requiring Python 3.11.

    Cargo emits these fields as single-line, unescaped ASCII strings. Reject
    other spellings instead of trying to implement a general TOML parser.
    """
    result = {}
    for block in re.split(r"(?m)^\[\[package\]\]\s*$", data.decode())[1:]:
        fields = {}
        for key in ("name", "version", "source", "checksum"):
            lines = re.findall(r"(?m)^" + key + r"\s*=\s*(.+)$", block)
            if len(lines) > 1:
                raise ValueError(f"duplicate Cargo.lock package field: {key}")
            if lines:
                if not re.fullmatch(r'"[\x20-\x21\x23-\x5b\x5d-\x7e]*"', lines[0]):
                    raise ValueError(f"unsupported Cargo.lock field spelling: {key}")
                fields[key] = json.loads(lines[0])
        if fields.get("source") in REGISTRIES:
            if not re.fullmatch(r"[0-9a-f]{64}", fields.get("checksum", "")):
                raise ValueError("registry package is missing a Cargo.lock checksum")
            identity = (fields["name"], fields["version"], fields["source"])
            if identity in result:
                raise ValueError("duplicate registry package identity in Cargo.lock")
            result[identity] = fields["checksum"]
    return result


def registry_files(package, checksums):
    root = Path(package["manifest_path"]).parent
    name = f"{package['name']}-{package['version']}"
    # Cargo's registry layout is src/<registry>/<crate> alongside cache/<registry>.
    # Read attribution from the verified original .crate, not mutable extracted files.
    archive = root.parent.parent.parent / "cache" / root.parent.name / f"{name}.crate"
    data = archive.read_bytes()
    expected = checksums.get((package["name"], package["version"], package["source"]))
    if expected is None or sha256(data) != expected:
        raise ValueError(f"{name}: registry archive differs from Cargo.lock")
    result = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
        for member in tar:
            path = safe_relative(member.name)
            if not path.parts or path.parts[0] != name:
                raise ValueError(f"{name}: unexpected archive member root")
            if member.isdir():
                continue
            if not member.isfile() or len(path.parts) < 2:
                raise ValueError(f"{name}: unsupported archive member type")
            relative = PurePosixPath(*path.parts[1:]).as_posix()
            if relative in result:
                raise ValueError(f"{name}: duplicate archive member")
            result[relative] = tar.extractfile(member).read()
    if result.get("Cargo.toml") != Path(package["manifest_path"]).read_bytes():
        raise ValueError(f"{name}: extracted package manifest differs from locked archive")
    return result, expected


def source_header(data, suffix):
    """Preserve the leading comment preamble verbatim when it has attribution."""
    text = data[:65536].decode("utf-8", errors="replace").lstrip("\ufeff")
    position = 0
    while position < len(text):
        start = position
        while position < len(text) and text[position].isspace():
            position += 1
        if text.startswith("/*", position):
            end = text.find("*/", position + 2)
            if end < 0:
                raise ValueError("unterminated or oversized leading source comment")
            position = end + 2
        elif text.startswith("//", position) or (
            suffix in {".py", ".pl", ".s"} and text.startswith("#", position)
        ) or (suffix == ".asm" and text.startswith(";", position)):
            end = text.find("\n", position)
            position = len(text) if end < 0 else end + 1
        else:
            position = start
            break
    header = text[:position].strip()
    if not ATTRIBUTION.search(header):
        return None
    if "\ufffd" in header:
        raise ValueError("attribution header is not valid UTF-8")
    return header + "\n"


def original_package_fields(files):
    """Read only reviewed scalar identity fields, without a general TOML parser."""
    original = files.get("Cargo.toml.orig", b"").decode("utf-8")
    blocks = re.findall(r"(?ms)^\[package\][ \t]*\r?\n(.*?)(?=^\[|\Z)", original)
    if len(blocks) != 1:
        raise ValueError("license fallback requires one original package manifest")
    result = {}
    for key in ("name", "version", "repository", "license", "repository.workspace", "license.workspace"):
        values = re.findall(r"(?m)^" + re.escape(key) + r"\s*=\s*(.+)$", blocks[0])
        if len(values) > 1:
            raise ValueError("duplicate license fallback identity field")
        if values:
            result[key] = values[0].strip()
    return result


def checked_license_identity(package, files, repository, commit, path_in_vcs, inherited):
    if package.get("repository") != repository or package.get("license") != "MIT":
        raise ValueError("license fallback repository or license identity changed")
    fields = original_package_fields(files)
    if fields.get("name") != json.dumps(package["name"]) or fields.get("version") != json.dumps(package["version"]):
        raise ValueError("license fallback original package identity changed")
    if inherited:
        if fields.get("license.workspace") != "true" or fields.get("repository.workspace") != "true":
            raise ValueError("license fallback requires explicit workspace inheritance")
    elif fields.get("license") != '"MIT"' or fields.get("repository") != json.dumps(repository):
        raise ValueError("license fallback original repository or license changed")
    try:
        vcs = json.loads(files[".cargo_vcs_info.json"])
    except (KeyError, ValueError, UnicodeDecodeError) as error:
        raise ValueError("license fallback requires archived VCS identity") from error
    if (vcs.get("git", {}).get("sha1") != commit
            or vcs.get("git", {}).get("dirty", False) is not False
            or vcs.get("path_in_vcs") != path_in_vcs):
        raise ValueError("license fallback archived VCS identity changed")


def fallback_license(package, files, checksums, inventory):
    identity = (package["name"], package["version"])
    if identity in WORKSPACE_LICENSE_PACKAGES:
        checked_license_identity(package, files, JSONSCHEMA_REPOSITORY,
                                 JSONSCHEMA_LICENSE_COMMIT, WORKSPACE_LICENSE_PACKAGES[identity], True)
        providers = [candidate for candidate in inventory
                     if candidate["name"] == "jsonschema" and candidate["version"] == "0.60.1"
                     and candidate.get("source") == package.get("source")]
        if len(providers) != 1:
            raise ValueError("license fallback requires the locked jsonschema 0.60.1 provider")
        provider = providers[0]
        provider_files, provider_checksum = registry_files(provider, checksums)
        checked_license_identity(provider, provider_files, JSONSCHEMA_REPOSITORY,
                                 JSONSCHEMA_LICENSE_COMMIT, "crates/jsonschema", True)
        data = provider_files.get("LICENSE", b"")
        if sha256(data) != JSONSCHEMA_LICENSE_SHA256:
            raise ValueError("license fallback workspace license text changed")
        return {
            "path": "workspace/jsonschema-0.60.1/LICENSE", "sha256": sha256(data), "text": data.decode("utf-8"),
            "origin": {
                "kind": "locked-workspace-license", "repository": JSONSCHEMA_REPOSITORY,
                "commit": JSONSCHEMA_LICENSE_COMMIT,
                "url": f"{JSONSCHEMA_REPOSITORY}/blob/{JSONSCHEMA_LICENSE_COMMIT}/LICENSE",
                "workspace_manifest_url": f"{JSONSCHEMA_REPOSITORY}/blob/{JSONSCHEMA_LICENSE_COMMIT}/Cargo.toml",
                "workspace_manifest_git_blob": "86bc8ad03d6c12e66ff40ab9af31ec16f41f1124",
                "provider": {"name": "jsonschema", "version": "0.60.1", "source": provider["source"],
                             "registry_archive_sha256": provider_checksum, "path": "LICENSE"},
            },
        }
    if identity in UPSTREAM_LICENSE_PACKAGES:
        checked_license_identity(package, files, SIMD_REPOSITORY,
                                 SIMD_LICENSE_COMMIT, UPSTREAM_LICENSE_PACKAGES[identity], False)
        source = SUPPLEMENT_ROOT / SIMD_LICENSE_FILE
        if source.is_symlink() or not source.is_file():
            raise ValueError("license fallback pinned upstream license is missing or unsafe")
        data = source.read_bytes()
        if sha256(data) != SIMD_LICENSE_SHA256:
            raise ValueError("license fallback pinned upstream license text changed")
        return {
            "path": "upstream/Nugine-simd/LICENSE", "sha256": sha256(data), "text": data.decode("utf-8"),
            "origin": {
                "kind": "pinned-upstream-license", "repository": SIMD_REPOSITORY,
                "commit": SIMD_LICENSE_COMMIT, "path": "LICENSE",
                "url": f"{SIMD_REPOSITORY}/blob/{SIMD_LICENSE_COMMIT}/LICENSE",
                "git_blob": "2c7eabb59093ea117d73b20effdd0414f87b2af2",
                "vendored_path": SIMD_LICENSE_FILE,
            },
        }
    return None


def collect_package(package, checksums, inventory=()):
    name, version = package["name"], package["version"]
    label = f"{name} {version}"
    if package.get("source") not in REGISTRIES:
        raise ValueError(f"{label}: non-crates.io dependency needs explicit notice review")
    root = Path(package["manifest_path"]).parent
    files, archive_checksum = registry_files(package, checksums)
    declared = package.get("license_file")
    if declared:
        declared_path = Path(declared)
        if declared_path.is_absolute():
            try:
                declared = declared_path.relative_to(root).as_posix()
            except ValueError as error:
                raise ValueError(f"{label}: license-file lies outside the package") from error
    if declared and declared not in files:
        raise ValueError(f"{label}: declared license-file missing from locked registry archive")
    documents, headers = [], {}
    for relative, data in sorted(files.items()):
        path = PurePosixPath(relative)
        is_notice = (
            NOTICE_NAME.match(path.name)
            or any(part.lower() in {"licenses", "licences"} for part in path.parts[:-1])
            or relative == declared
            or relative in {"REUSE.toml", ".reuse/dep5"}
        )
        is_source = path.suffix.lower() in SOURCE_SUFFIXES
        if not is_notice and not is_source:
            continue
        header = source_header(data, path.suffix.lower()) if is_source else None
        if not is_notice and header is None:
            continue
        expected_hash = sha256(data)
        if is_notice:
            text = data.decode("utf-8")
            if not text.strip():
                raise ValueError(f"{label}: empty attribution file: {relative}")
            documents.append({"path": relative, "sha256": expected_hash, "text": text})
        if header:
            entry = headers.setdefault(header, [])
            entry.append({"path": relative, "sha256": expected_hash})
    license_documents = [document for document in documents if (
        re.match(r"^(?:licen[cs]e|unlicense|copying)(?:$|[._-])", PurePosixPath(document["path"]).name, re.I)
        or any(part.lower() in {"licenses", "licences"} for part in PurePosixPath(document["path"]).parts[:-1])
        or document["path"] == declared
    )]
    if not license_documents:
        supplement = fallback_license(package, files, checksums, inventory)
        if supplement is None:
            raise ValueError(f"{label}: no supplied license text; SPDX metadata alone is insufficient")
        documents.append(supplement)
    if not package.get("license") and not declared:
        raise ValueError(f"{label}: neither license metadata nor license-file is declared")
    if name == "aws-lc-sys":
        by_path = {document["path"]: document["text"] for document in documents}
        missing = set(AWS_REQUIRED) - by_path.keys()
        if missing:
            raise ValueError(f"{label}: missing bundled AWS-LC notices: {sorted(missing)}")
        if any(component not in by_path["aws-lc/LICENSE"] for component in AWS_COMPONENTS):
            raise ValueError(f"{label}: bundled AWS-LC component attribution changed; review required")
        for prefix in ("aws-lc/third_party/s2n-bignum/", "aws-lc/third_party/jitterentropy/"):
            if not any(source["path"].startswith(prefix) for sources in headers.values() for source in sources):
                raise ValueError(f"{label}: missing bundled component source attribution: {prefix}")
    return {
        "name": name, "version": version, "source": package["source"],
        "license_expression": package.get("license"),
        "registry_archive_sha256": archive_checksum, "documents": documents,
        "source_headers": [{"text": text, "sha256": sha256(text.encode()), "sources": sources}
                           for text, sources in sorted(headers.items(), key=lambda item: item[1][0]["path"])],
    }


def make_bundle(metadata, target, lock_bytes, rust_version, stdlib_bytes):
    if b"Copyright notices for The Rust Standard Library" not in stdlib_bytes:
        raise ValueError("rust-docs COPYRIGHT-library.html is missing or unrecognized")
    workspace = set(metadata["workspace_members"])
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    packages = sorted((package for package in metadata["packages"]
                       if package["id"] in resolved and package["id"] not in workspace),
                      key=lambda package: (package["name"], package["version"], package["id"]))
    if not packages:
        raise ValueError("empty dependency resolution; refusing an empty notice bundle")
    checksums = locked_checksums(lock_bytes)
    collected = [collect_package(package, checksums, packages) for package in packages]
    text = [
        "THIRD-PARTY NOTICES FOR RUSTA-PITEST\n",
        f"Target: {target}\nRust toolchain: {rust_version}\n",
        "Scope: the locked target-filtered Cargo resolution, conservatively including\n"
        "build and optional packages. Inclusion does not assert that a package is linked.\n"
        "License expressions are upstream metadata, not a choice of licensing terms.\n"
        "Supplied license/notice files and leading source attribution headers follow.\n"
        "Attributions come from registry archives verified against Cargo.lock or\n"
        "explicitly pinned upstream supplements; origins are recorded per document.\n"
        "The exact toolchain's standard-library and bundled dependency notices are in\n"
        "RUST-STDLIB-LICENSE.html. System libraries supplied by the operating system\n"
        "are not copied into these release archives.\n",
    ]
    manifest_packages = []
    for package in collected:
        text.append("\n" + "=" * 78 + "\n")
        text.append(f"{package['name']} {package['version']}\n")
        text.append(f"Declared license: {package['license_expression'] or 'see supplied license-file'}\n")
        text.append(f"Registry archive SHA-256: {package['registry_archive_sha256']}\n")
        for document in package["documents"]:
            text.append(f"\n--- {document['path']} (SHA-256 {document['sha256']}) ---\n")
            if document.get("origin"):
                text.append(f"Origin: {document['origin']['url']}\n")
            text.append(document["text"] + ("" if document["text"].endswith("\n") else "\n"))
        for header in package["source_headers"]:
            text.append("\n--- Source attribution: " + ", ".join(source["path"] for source in header["sources"]) + " ---\n")
            text.append(header["text"])
        manifest_packages.append({
            **{key: value for key, value in package.items() if key not in {"documents", "source_headers"}},
            "documents": [{key: value for key, value in document.items() if key != "text"} for document in package["documents"]],
            "source_headers": [{key: value for key, value in header.items() if key != "text"} for header in package["source_headers"]],
        })
    notices = "".join(text).encode()
    manifest = {
        "schema_version": 1, "generator": "scripts/generate-notices.py",
        "target": target, "cargo_lock_sha256": sha256(lock_bytes),
        "scope": "target-filtered locked resolution including build and optional packages",
        "packages": manifest_packages,
        "rust_standard_library": {
            "rustc_version": rust_version, "file": "RUST-STDLIB-LICENSE.html",
            "sha256": sha256(stdlib_bytes),
            "origin": "active rust-docs component: share/doc/rust/COPYRIGHT-library.html",
        },
        "notices_sha256": sha256(notices),
    }
    return {
        "THIRD-PARTY-NOTICES.txt": notices,
        "THIRD-PARTY-MANIFEST.json": json_bytes(manifest),
        "RUST-STDLIB-LICENSE.html": stdlib_bytes,
    }


def write_or_check(directory, bundle, check):
    if directory.is_symlink():
        raise ValueError("output directory must not be a symlink")
    if not check:
        directory.mkdir(parents=True, exist_ok=True)
    for name, data in bundle.items():
        path = directory / name
        if path.is_symlink():
            raise ValueError(f"notice output must not be a symlink: {name}")
        if check:
            if not path.is_file() or path.read_bytes() != data:
                raise ValueError(f"notice bundle is missing or stale: {name}")
        else:
            path.write_bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Cargo target triple; defaults to the rustc host")
    parser.add_argument("--output-dir", type=Path, default=Path("target/notices"))
    parser.add_argument("--check", action="store_true", help="fail if generated output differs")
    parser.add_argument("--offline", action="store_true", help="require dependency sources already cached")
    args = parser.parse_args()
    rust_verbose = subprocess.check_output(["rustc", "-vV"], text=True)
    target = args.target or next(line.split(": ", 1)[1] for line in rust_verbose.splitlines() if line.startswith("host: "))
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", target):
        raise ValueError("expected a Cargo target triple")
    command = ["cargo", "metadata", "--locked", "--filter-platform", target, "--format-version", "1"]
    if args.offline:
        command.append("--offline")
    metadata = json.loads(subprocess.check_output(command))
    root = Path(metadata["workspace_root"])
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    stdlib = sysroot / "share/doc/rust/COPYRIGHT-library.html"
    if not stdlib.is_file():
        raise ValueError("install the active toolchain's rust-docs component (rustup component add rust-docs)")
    bundle = make_bundle(metadata, target, (root / "Cargo.lock").read_bytes(),
                         rust_verbose.splitlines()[0], stdlib.read_bytes())
    write_or_check(args.output_dir, bundle, args.check)
    manifest = json.loads(bundle["THIRD-PARTY-MANIFEST.json"])
    verb = "Verified" if args.check else "Generated"
    print(f"{verb} notices for {len(manifest['packages'])} registry packages and Rust stdlib ({target})")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Notice generation failed: {error}", file=sys.stderr)
        sys.exit(1)
