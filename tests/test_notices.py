"""Notice generation must retain supplied attribution and fail closed on gaps."""

import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "generate_notices", Path(__file__).resolve().parents[1] / "scripts/generate-notices.py"
)
NOTICES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(NOTICES)
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
STDLIB = b"<html>Copyright notices for The Rust Standard Library\nLicense text</html>"


class NoticeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rusta-notices-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def fixture(self, files=None, name="example", extra_member=None):
        manifest = b'[package]\nname = "example"\nversion = "1.0.0"\nlicense = "MIT"\n'
        supplied = {
            "Cargo.toml": manifest,
            "LICENSE": b"Copyright Example Authors\nPermission is hereby granted to use this software.\n",
            "src/lib.rs": b"// Copyright Additional Author\n// Licensed under MIT.\npub fn example() {}\n",
        } if files is None else {"Cargo.toml": manifest, **files}
        prefix = f"{name}-1.0.0"
        source = self.root / "registry/src/test-registry" / prefix
        source.mkdir(parents=True)
        (source / "Cargo.toml").write_bytes(manifest)
        archive = self.root / "registry/cache/test-registry" / f"{prefix}.crate"
        archive.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive, "w:gz") as tar:
            for relative, data in sorted(supplied.items()):
                entry = tarfile.TarInfo(f"{prefix}/{relative}")
                entry.size = len(data)
                tar.addfile(entry, io.BytesIO(data))
            if extra_member:
                entry = tarfile.TarInfo(extra_member)
                entry.size = 1
                tar.addfile(entry, io.BytesIO(b"x"))
        checksum = NOTICES.sha256(archive.read_bytes())
        package = {
            "id": f"{REGISTRY}#{name}@1.0.0", "name": name, "version": "1.0.0",
            "source": REGISTRY, "license": "MIT", "license_file": None,
            "manifest_path": str(source / "Cargo.toml"),
        }
        lock = (
            f'version = 4\n\n[[package]]\nname = "{name}"\nversion = "1.0.0"\n'
            f'source = "{REGISTRY}"\nchecksum = "{checksum}"\n'
        ).encode()
        metadata = {
            "workspace_members": ["workspace-root"], "packages": [package],
            "resolve": {"nodes": [{"id": package["id"]}]},
        }
        return package, lock, metadata, archive

    def bundle(self, metadata, lock):
        return NOTICES.make_bundle(metadata, "example-target", lock, "rustc test", STDLIB)

    def test_deterministic_bundle_retains_text_headers_and_source_hashes(self):
        _, lock, metadata, _ = self.fixture()
        first = self.bundle(metadata, lock)
        self.assertEqual(first, self.bundle(metadata, lock))
        text = first["THIRD-PARTY-NOTICES.txt"].decode()
        self.assertIn("Copyright Example Authors", text)
        self.assertIn("Copyright Additional Author", text)
        self.assertNotIn("pub fn example", text)
        self.assertNotIn(str(self.root), text)
        manifest = json.loads(first["THIRD-PARTY-MANIFEST.json"])
        self.assertEqual(manifest["cargo_lock_sha256"], NOTICES.sha256(lock))
        self.assertEqual(manifest["notices_sha256"], NOTICES.sha256(first["THIRD-PARTY-NOTICES.txt"]))
        self.assertNotIn(str(self.root), json.dumps(manifest))
        self.assertEqual(first["RUST-STDLIB-LICENSE.html"], STDLIB)

    def test_spdx_metadata_without_actual_license_is_rejected(self):
        _, lock, metadata, _ = self.fixture({"NOTICE": b"Copyright Example Authors\n"})
        with self.assertRaisesRegex(ValueError, "no supplied license text"):
            self.bundle(metadata, lock)

    def test_nested_license_notice_and_authors_are_preserved(self):
        _, lock, metadata, _ = self.fixture({
            "licenses/MIT.txt": b"Copyright Main Authors\nPermission is hereby granted.\n",
            "vendor/subcomponent/NOTICE.txt": b"Copyright Subcomponent Authors\n",
            "AUTHORS": b"Contributing Author\n",
        })
        bundle = self.bundle(metadata, lock)
        text = bundle["THIRD-PARTY-NOTICES.txt"].decode()
        for marker in ("Copyright Main Authors", "Copyright Subcomponent Authors", "Contributing Author"):
            self.assertIn(marker, text)

    def test_tampered_archive_and_manifest_are_rejected(self):
        package, lock, metadata, archive = self.fixture()
        original = archive.read_bytes()
        archive.write_bytes(original + b"modified")
        with self.assertRaisesRegex(ValueError, "archive differs from Cargo.lock"):
            self.bundle(metadata, lock)
        archive.write_bytes(original)
        Path(package["manifest_path"]).write_text("modified")
        with self.assertRaisesRegex(ValueError, "manifest differs from locked archive"):
            self.bundle(metadata, lock)

    def test_unsafe_archive_member_is_rejected_without_extraction(self):
        _, lock, metadata, _ = self.fixture(extra_member="example-1.0.0/../../escape")
        with self.assertRaisesRegex(ValueError, "unsafe package-relative path"):
            self.bundle(metadata, lock)
        self.assertFalse((self.root / "escape").exists())

    def test_aws_lc_cannot_lose_bundled_component_notices(self):
        _, lock, metadata, _ = self.fixture(name="aws-lc-sys")
        with self.assertRaisesRegex(ValueError, "missing bundled AWS-LC notices"):
            self.bundle(metadata, lock)

    def test_stale_bundle_and_missing_runtime_notices_are_rejected(self):
        _, lock, metadata, _ = self.fixture()
        bundle = self.bundle(metadata, lock)
        output = self.root / "output"
        NOTICES.write_or_check(output, bundle, False)
        NOTICES.write_or_check(output, bundle, True)
        (output / "THIRD-PARTY-NOTICES.txt").write_text("stale")
        with self.assertRaisesRegex(ValueError, "missing or stale"):
            NOTICES.write_or_check(output, bundle, True)
        with self.assertRaisesRegex(ValueError, "missing or unrecognized"):
            NOTICES.make_bundle(metadata, "example-target", lock, "rustc test", b"")

    def test_nonregistry_dependency_requires_review(self):
        package, lock, metadata, _ = self.fixture()
        package["source"] = "git+https://example.invalid/dependency"
        with self.assertRaisesRegex(ValueError, "non-crates.io dependency"):
            self.bundle(metadata, lock)


if __name__ == "__main__":
    unittest.main()
