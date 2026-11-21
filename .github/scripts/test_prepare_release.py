import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from prepare_release import MAX_COMMITS, MAX_FACT_BYTES, prepare, public_subject


class PrepareReleaseTests(unittest.TestCase):
    def command(self, root: Path, *arguments: str) -> str:
        return subprocess.check_output(
            ["git", "-C", str(root), *arguments], encoding="utf-8", stderr=subprocess.PIPE
        ).strip()

    def fixture(self, root: Path, version: str = "0.1.1") -> str:
        self.command(root, "init", "--initial-branch=main")
        self.command(root, "config", "user.name", "Test")
        self.command(root, "config", "user.email", "test@example.invalid")
        self.command(root, "config", "commit.gpgsign", "false")
        self.command(root, "config", "core.hooksPath", str(root / "unused-hooks"))
        (root / "Cargo.toml").write_text(
            '[workspace.package]\nversion = "0.1.0"\n', encoding="utf-8"
        )
        self.command(root, "add", "Cargo.toml")
        self.command(root, "commit", "-m", "initial version")
        self.command(root, "tag", "v0.1.0")
        (root / "Cargo.toml").write_text(
            f'[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
        )
        self.command(root, "add", "Cargo.toml")
        self.command(root, "commit", "-m", "feat: add installer")
        source = self.command(root, "rev-parse", "HEAD")
        self.command(root, "tag", f"v{version}")
        self.command(root, "update-ref", "refs/remotes/origin/main", source)
        return source

    def test_validates_lightweight_tag_and_collects_only_new_subjects(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            facts = prepare(root, "v0.1.1", source)
            self.assertEqual(facts["source_sha"], source)
            self.assertEqual(facts["previous_tag"], "v0.1.0")
            self.assertEqual(facts["commit_count"], 1)
            self.assertEqual(facts["commits"][0]["subject"], "feat: add installer")
            self.assertFalse(facts["prerelease"])
            self.assertNotIn("example.invalid", json.dumps(facts))

    def test_annotated_tag_prerelease_and_isolated_cli(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version = "0.2.0-beta.1+build.2"
            source = self.fixture(root, version)
            tag = f"v{version}"
            self.command(root, "tag", "-d", tag)
            self.command(root, "tag", "-a", tag, "-m", "release")
            object_id = self.command(root, "rev-parse", f"refs/tags/{tag}")
            self.assertTrue(prepare(root, tag, object_id)["prerelease"])
            script = Path(__file__).with_name("prepare_release.py")
            result = subprocess.run(
                [sys.executable, "-I", "-B", str(script), "--root", str(root),
                 "--tag", tag, "--expected-sha", object_id, "--output", str(root / "facts.json")],
                capture_output=True, encoding="utf-8",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads((root / "facts.json").read_text())["source_sha"], source)

    def test_mismatched_version_or_trigger_sha_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            self.command(root, "tag", "v0.2.0")
            with self.assertRaisesRegex(ValueError, "workspace"):
                prepare(root, "v0.2.0", source)
            previous = self.command(root, "rev-parse", "v0.1.0")
            with self.assertRaisesRegex(ValueError, "triggering"):
                prepare(root, "v0.1.1", previous)

    def test_rejects_tag_commit_not_in_main(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            self.command(root, "update-ref", "refs/remotes/origin/main", self.command(root, "rev-parse", "v0.1.0"))
            with self.assertRaisesRegex(ValueError, "origin/main"):
                prepare(root, "v0.1.1", source)

    def test_first_release_and_invalid_previous_tags(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            self.command(root, "tag", "-d", "v0.1.0")
            self.command(root, "tag", "v9-invalid")
            self.command(root, "tag", "v0.1.1+alias")
            facts = prepare(root, "v0.1.1", source)
            self.assertIsNone(facts["previous_tag"])
            self.assertEqual(facts["commit_count"], 2)

    def test_rejects_invalid_semver_and_ref_injection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            for tag in ("main", "--all", "v01.2.3", "v1.2.3-01", "v1.2.3/escape", "v1.2.3\ntag=x"):
                with self.subTest(tag=tag), self.assertRaises(ValueError):
                    prepare(root, tag, source)

    def test_multibyte_commit_history_obeys_total_fact_byte_limit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = self.fixture(root)
            self.command(root, "tag", "-d", "v0.1.1")
            for number in range(100):
                self.command(root, "commit", "--allow-empty", "-m", f"fix {number}: " + "测" * 240)
            source = self.command(root, "rev-parse", "HEAD")
            self.command(root, "tag", "v0.1.1")
            self.command(root, "update-ref", "refs/remotes/origin/main", source)
            data = prepare(root, "v0.1.1", source)
            self.assertEqual(data["commit_count"], 101)
            self.assertLess(len(data["commits"]), 100)
            self.assertLessEqual(len(json.dumps(data, ensure_ascii=False, indent=2).encode()), MAX_FACT_BYTES)

    def test_subjects_are_bounded_and_remove_identities_and_tokens(self):
        subject = "fix: email test@example.invalid by @someone https://example.invalid/secret sk-1234567890123456 ghp_1234567890\x1b\n"
        safe = public_subject(subject)
        for sensitive in ("test@example", "@someone", "https://", "sk-47fc", "ghp_123", "\x1b", "\n"):
            self.assertNotIn(sensitive, safe)
        self.assertLessEqual(len(public_subject("a" * 400)), 240)
        self.assertEqual(MAX_COMMITS, 100)


if __name__ == "__main__":
    unittest.main()
