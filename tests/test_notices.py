"""Notice generation must retain supplied attribution and fail closed on gaps."""

import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


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

    def fixture(self, files=None, name="example", extra_member=None, version="1.0.0", repository=None):
        manifest = f'[package]\nname = "{name}"\nversion = "{version}"\nlicense = "MIT"\n'.encode()
        if repository:
            manifest += f'repository = "{repository}"\n'.encode()
        supplied = {
            "Cargo.toml": manifest,
            "LICENSE": b"Copyright Example Authors\nPermission is hereby granted to use this software.\n",
            "src/lib.rs": b"// Copyright Additional Author\n// Licensed under MIT.\npub fn example() {}\n",
        } if files is None else {"Cargo.toml": manifest, **files}
        prefix = f"{name}-{version}"
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
            "id": f"{REGISTRY}#{name}@{version}", "name": name, "version": version,
            "source": REGISTRY, "license": "MIT", "license_file": None,
            "manifest_path": str(source / "Cargo.toml"),
        }
        if repository:
            package["repository"] = repository
        lock = (
            f'version = 4\n\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
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

    def workspace_fixture(self, child="jsonschema-regex", child_changes=None, provider_changes=None):
        repository = NOTICES.JSONSCHEMA_REPOSITORY
        commit = NOTICES.JSONSCHEMA_LICENSE_COMMIT

        def inherited_files(name):
            return {
                "Cargo.toml.orig": (
                    f'[package]\nname = "{name}"\nversion = "0.60.1"\n'
                    'license.workspace = true\nrepository.workspace = true\n'
                ).encode(),
                ".cargo_vcs_info.json": json.dumps({
                    "git": {"sha1": commit}, "path_in_vcs": "crates/" + name,
                }).encode(),
            }

        child_files = inherited_files(child)
        child_files.update(child_changes or {})
        package, child_lock, metadata, _ = self.fixture(
            child_files, name=child, version="0.60.1", repository=repository,
        )
        provider_files = inherited_files("jsonschema")
        # Synthetic test attribution checks provenance mechanics, not upstream terms.
        provider_files["LICENSE"] = b"Copyright Synthetic Fixture Authors\nPermission is hereby granted.\n"
        provider_files.update(provider_changes or {})
        provider, provider_lock, _, _ = self.fixture(
            provider_files, name="jsonschema", version="0.60.1", repository=repository,
        )
        metadata["packages"].append(provider)
        metadata["resolve"]["nodes"].append({"id": provider["id"]})
        combined_lock = child_lock + b"\n[[package]]" + provider_lock.split(b"[[package]]", 1)[1]
        return package, provider, metadata, combined_lock

    def test_workspace_license_fallback_records_locked_source_provenance(self):
        for index, child in enumerate(("jsonschema-regex", "jsonschema-value")):
            with self.subTest(child=child), patch.object(self, "root", self.root / str(index)):
                self.root.mkdir()
                package, provider, metadata, lock = self.workspace_fixture(child)
                expected = NOTICES.sha256(b"Copyright Synthetic Fixture Authors\nPermission is hereby granted.\n")
                with patch.object(NOTICES, "JSONSCHEMA_LICENSE_SHA256", expected):
                    bundle = self.bundle(metadata, lock)
                manifest = json.loads(bundle["THIRD-PARTY-MANIFEST.json"])
                child_notice = next(item for item in manifest["packages"] if item["name"] == package["name"])
                document = next(item for item in child_notice["documents"] if "origin" in item)
                self.assertEqual(document["sha256"], expected)
                origin = document["origin"]
                self.assertEqual(origin["kind"], "locked-workspace-license")
                self.assertEqual(origin["commit"], NOTICES.JSONSCHEMA_LICENSE_COMMIT)
                self.assertEqual(origin["provider"]["name"], provider["name"])
                self.assertEqual(origin["provider"]["registry_archive_sha256"], NOTICES.locked_checksums(lock)[
                    (provider["name"], provider["version"], provider["source"])
                ])
                self.assertNotIn(str(self.root), bundle["THIRD-PARTY-MANIFEST.json"].decode())

    def test_workspace_fallback_rejects_changed_inheritance_vcs_or_license(self):
        variants = [
            ({"Cargo.toml.orig": b'[package]\nname = "jsonschema-regex"\nversion = "0.60.1"\nlicense = "MIT"\nrepository.workspace = true\n'}, None),
            ({".cargo_vcs_info.json": b'{"git":{"sha1":"wrong"},"path_in_vcs":"crates/jsonschema-regex"}'}, None),
            (None, {".cargo_vcs_info.json": b'{"git":{"sha1":"wrong"},"path_in_vcs":"crates/jsonschema"}'}),
            (None, {"LICENSE": b"Changed license text\n"}),
        ]
        expected = NOTICES.sha256(b"Copyright Synthetic Fixture Authors\nPermission is hereby granted.\n")
        for index, (child_changes, provider_changes) in enumerate(variants):
            with self.subTest(index=index), patch.object(self, "root", self.root / str(index)):
                self.root.mkdir()
                _, _, metadata, lock = self.workspace_fixture(
                    child_changes=child_changes, provider_changes=provider_changes,
                )
                with patch.object(NOTICES, "JSONSCHEMA_LICENSE_SHA256", expected):
                    with self.assertRaisesRegex(ValueError, "license fallback"):
                        self.bundle(metadata, lock)

    def test_workspace_fallback_requires_same_version_repository_and_locked_provider(self):
        expected = NOTICES.sha256(b"Copyright Synthetic Fixture Authors\nPermission is hereby granted.\n")
        for index, change in enumerate(("missing", "version", "repository", "source")):
            with self.subTest(change=change), patch.object(self, "root", self.root / str(index)):
                self.root.mkdir()
                package, provider, metadata, lock = self.workspace_fixture()
                if change == "missing":
                    metadata["packages"].remove(provider)
                elif change == "version":
                    provider["version"] = "0.60.2"
                elif change == "repository":
                    provider["repository"] = "https://example.invalid/unrelated"
                else:
                    provider["source"] = "registry+https://example.invalid/other"
                with patch.object(NOTICES, "JSONSCHEMA_LICENSE_SHA256", expected):
                    with self.assertRaisesRegex(ValueError, "license fallback"):
                        NOTICES.collect_package(package, NOTICES.locked_checksums(lock), metadata["packages"])

    def simd_fixture(self, name="uuid-simd", changes=None, version="0.8.0"):
        files = {
            "Cargo.toml.orig": (
                f'[package]\nname = "{name}"\nversion = "{version}"\n'
                f'license = "MIT"\nrepository = "{NOTICES.SIMD_REPOSITORY}"\n'
            ).encode(),
            ".cargo_vcs_info.json": json.dumps({
                "git": {"sha1": NOTICES.SIMD_LICENSE_COMMIT}, "path_in_vcs": "crates/" + name,
            }).encode(),
        }
        files.update(changes or {})
        return self.fixture(files, name=name, version=version, repository=NOTICES.SIMD_REPOSITORY)

    def test_pinned_upstream_license_is_verbatim_and_has_source_identity(self):
        for index, name in enumerate(("uuid-simd", "vsimd")):
            with self.subTest(name=name), patch.object(self, "root", self.root / str(index)):
                self.root.mkdir()
                _, lock, metadata, _ = self.simd_fixture(name)
                bundle = self.bundle(metadata, lock)
                manifest = json.loads(bundle["THIRD-PARTY-MANIFEST.json"])
                document = manifest["packages"][0]["documents"][0]
                self.assertEqual(document["sha256"], NOTICES.SIMD_LICENSE_SHA256)
                self.assertEqual(document["origin"]["commit"], NOTICES.SIMD_LICENSE_COMMIT)
                self.assertEqual(document["origin"]["url"],
                                 f"{NOTICES.SIMD_REPOSITORY}/blob/{NOTICES.SIMD_LICENSE_COMMIT}/LICENSE")
                license_text = (NOTICES.SUPPLEMENT_ROOT / NOTICES.SIMD_LICENSE_FILE).read_bytes()
                self.assertIn(license_text, bundle["THIRD-PARTY-NOTICES.txt"])
                self.assertNotIn(str(self.root), bundle["THIRD-PARTY-MANIFEST.json"].decode())

    def test_pinned_upstream_license_missing_changed_or_wrong_vcs_fails_closed(self):
        original = (NOTICES.SUPPLEMENT_ROOT / NOTICES.SIMD_LICENSE_FILE).read_bytes()
        for index, change in enumerate(("missing", "text", "vcs", "repository", "version")):
            with self.subTest(change=change), patch.object(self, "root", self.root / str(index)):
                self.root.mkdir()
                changes = {".cargo_vcs_info.json": b'{"git":{"sha1":"wrong"},"path_in_vcs":"crates/uuid-simd"}'} if change == "vcs" else None
                package, lock, metadata, _ = self.simd_fixture(
                    changes=changes, version="0.8.1" if change == "version" else "0.8.0",
                )
                if change == "repository":
                    package["repository"] = "https://example.invalid/unrelated"
                supplement_root = self.root / "supplement"
                source = supplement_root / NOTICES.SIMD_LICENSE_FILE
                source.parent.mkdir(parents=True)
                if change != "missing":
                    source.write_bytes(original + (b"changed" if change == "text" else b""))
                with patch.object(NOTICES, "SUPPLEMENT_ROOT", supplement_root):
                    with self.assertRaises(ValueError):
                        self.bundle(metadata, lock)


if __name__ == "__main__":
    unittest.main()
