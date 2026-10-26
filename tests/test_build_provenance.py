"""Exercise the actual build script without Cargo dependencies or network access."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class BuildProvenanceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compiled = tempfile.TemporaryDirectory(prefix="rusta-build-script-")
        cls.addClassCleanup(cls.compiled.cleanup)
        cls.binary = Path(cls.compiled.name) / (
            "build-script.exe" if os.name == "nt" else "build-script"
        )
        subprocess.run(
            ["rustc", "--edition=2024", str(ROOT / "build.rs"), "-o", str(cls.binary)],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="rusta-provenance-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name).resolve()
        self.environment = os.environ.copy()
        for name in (
            "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE",
            "GIT_CONFIG", "GIT_CONFIG_COUNT", "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ):
            self.environment.pop(name, None)
        self.environment.update({
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CEILING_DIRECTORIES": str(self.directory),
        })

    def git(self, directory, *arguments):
        return subprocess.run(
            ["git", "-C", str(directory), *arguments],
            env=self.environment,
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        ).stdout.strip()

    def repository(self):
        repository = self.directory / "repository"
        repository.mkdir()
        self.git(repository, "init", "--initial-branch=main")
        (repository / "Cargo.toml").write_text("[package]\nname = 'fixture'\n")
        (repository / "src").mkdir()
        (repository / "src/main.rs").write_text("fn main() {}\n")
        self.git(repository, "add", "Cargo.toml", "src/main.rs")
        self.git(
            repository,
            "-c", "user.name=Build Test", "-c", "user.email=build@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-m", "Fixture",
        )
        return repository

    def build_identity(self, manifest_dir):
        environment = self.environment | {
            "CARGO_MANIFEST_DIR": str(manifest_dir),
            "TARGET": "fixture-target",
            "PROFILE": "release",
        }
        result = subprocess.run(
            [str(self.binary)],
            # Cargo supplies the package path explicitly; do not accidentally
            # rely on the caller's current directory for Git discovery.
            cwd=self.directory,
            env=environment,
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
        prefix = "cargo:rustc-env="
        return dict(
            line.removeprefix(prefix).split("=", 1)
            for line in result.stdout.splitlines()
            if line.startswith(prefix)
        )

    def test_repository_reports_revision_and_tracks_source_changes(self):
        repository = self.repository()
        identity = self.build_identity(repository)
        self.assertEqual(identity["RUSTA_GIT_REVISION"], self.git(repository, "rev-parse", "HEAD"))
        self.assertEqual(identity["RUSTA_SOURCE_DIRTY"], "false")
        (repository / "src/main.rs").write_text("fn main() { println!(\"changed\"); }\n")
        self.assertEqual(self.build_identity(repository)["RUSTA_SOURCE_DIRTY"], "true")

    def test_linked_worktree_reports_its_revision(self):
        repository = self.repository()
        worktree = self.directory / "worktree"
        self.git(repository, "worktree", "add", "--detach", str(worktree), "HEAD")
        identity = self.build_identity(worktree)
        self.assertEqual(identity["RUSTA_GIT_REVISION"], self.git(worktree, "rev-parse", "HEAD"))
        self.assertEqual(identity["RUSTA_SOURCE_DIRTY"], "false")

    def test_archive_outside_git_has_unknown_identity(self):
        archive = self.directory / "archive"
        archive.mkdir()
        identity = self.build_identity(archive)
        self.assertEqual(identity["RUSTA_GIT_REVISION"], "")
        self.assertEqual(identity["RUSTA_SOURCE_DIRTY"], "")

    def test_archive_inside_unrelated_git_does_not_borrow_its_identity(self):
        repository = self.repository()
        archive = repository / "downloaded-source"
        archive.mkdir()
        identity = self.build_identity(archive)
        self.assertEqual(identity["RUSTA_GIT_REVISION"], "")
        self.assertEqual(identity["RUSTA_SOURCE_DIRTY"], "")


if __name__ == "__main__":
    unittest.main()
