"""Test changelog validation and release preparation."""

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from changelog import SRC, read_entries

spec = importlib.util.spec_from_file_location("prepare", Path(__file__).with_name("prepare-changelog.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
SCRIPTS = Path(__file__).resolve().parent


class ChangelogTests(unittest.TestCase):
    def setUp(self):
        self.previous = Path.cwd()
        self.directory = tempfile.TemporaryDirectory()
        os.chdir(self.directory.name)
        SRC.mkdir(parents=True)
        self.path = SRC / "change.md"

    def tearDown(self):
        os.chdir(self.previous)
        self.directory.cleanup()

    def release(self, tag="v1.0.0"):
        with patch.object(sys, "argv", ["prepare-changelog.py", tag]):
            prepare.main()

    def check(self, label="false", changed="", status="0"):
        scripts = Path("scripts")
        scripts.mkdir(exist_ok=True)
        for name in ["changelog.py", "check-changelog.sh"]:
            shutil.copyfile(SCRIPTS / name, scripts / name)
        binary = Path("bin")
        binary.mkdir(exist_ok=True)
        git = binary / "git"
        git.write_text('#!/bin/sh\nprintf "%s" "$CHECK_DIFF"\nexit "$CHECK_STATUS"\n')
        git.chmod(0o755)
        env = dict(os.environ, PATH=f"{binary.resolve()}{os.pathsep}{os.environ['PATH']}",
                   NO_CHANGELOG_LABEL=label, BASE_REF="next", CHECK_DIFF=changed, CHECK_STATUS=status)
        return subprocess.run(["bash", "scripts/check-changelog.sh"], env=env,
                              capture_output=True, text=True)

    def test_valid_entries(self):
        lines = [
            "[BREAKING][param][lib,store] Added a required parameter.",
            "[FEATURE][cli] Added a command.",
            "[FIX][lib] Fixed a panic.",
        ]
        self.path.write_text("\n".join(lines) + "\n")
        self.assertEqual(read_entries(self.path), lines)

    def test_invalid_entries(self):
        for line in [
            "", "another-fragment", "[FIX][rust] Fixed a panic.",
            "[BREAKING][unknown][lib] Changed an API.",
            "[FEATURE][cli] ", "- [FIX][lib] Fixed a panic.",
            "[FIX][lib] Fixed a panic.\n\n",
        ]:
            with self.subTest(line=line):
                self.path.write_text(line)
                with self.assertRaises(ValueError):
                    read_entries(self.path)

    def test_nested_entries(self):
        self.path = SRC / "nested" / "change.md"
        self.path.parent.mkdir()
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        with self.assertRaises(ValueError):
            read_entries(self.path)

    def test_release_links_and_groups_entries(self):
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        linked = "[FEATURE][cli] Added a command ([#42](https://github.com/0xMiden/rust-sdk/pull/42))."
        (SRC / "existing.md").write_text(linked + "\n")
        with patch.object(prepare, "pr_of", return_value=42), patch.object(prepare.subprocess, "run"):
            self.release()
        output = Path(".changelog/v1.0.0/42.md").read_text()
        self.assertIn("- [FIX][lib] Fixed a panic ([#42](https://github.com/0xMiden/rust-sdk/pull/42)).\n", output)
        self.assertIn(f"- {linked}\n", output)
        self.assertEqual(output.count("- "), 2)

    def test_existing_destination_is_unchanged(self):
        dst = Path(".changelog/v1.0.0")
        dst.mkdir()
        existing = dst / "42.md"
        existing.write_text("Existing release notes.\n")
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        with self.assertRaisesRegex(SystemExit, "already exists"):
            self.release()
        self.assertEqual(existing.read_text(), "Existing release notes.\n")
        self.assertTrue(self.path.exists())

    def test_invalid_entries_do_not_write_release(self):
        self.path.write_text("another-fragment\n")
        with self.assertRaisesRegex(SystemExit, "Invalid entry"):
            self.release()
        self.assertFalse(Path(".changelog/v1.0.0").exists())
        self.assertTrue(self.path.exists())

    def test_missing_pr_does_not_write_release(self):
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        with patch.object(prepare, "pr_of", side_effect=SystemExit("No PR number")):
            with self.assertRaisesRegex(SystemExit, "No PR number"):
                self.release()
        self.assertFalse(Path(".changelog/v1.0.0").exists())

    def test_invalid_tag(self):
        with self.assertRaisesRegex(SystemExit, "Release tags"):
            self.release("../unreleased")

    def test_label_does_not_skip_validation(self):
        self.path.write_text("another-fragment\n")
        result = self.check(label="true")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid entry", result.stderr)

    def test_check_requires_a_change_or_label(self):
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        self.assertNotEqual(self.check().returncode, 0)
        self.assertEqual(self.check(label="true").returncode, 0)
        self.assertEqual(self.check(changed=str(self.path)).returncode, 0)

    def test_check_propagates_git_failure(self):
        self.path.write_text("[FIX][lib] Fixed a panic.\n")
        self.assertNotEqual(self.check(status="128").returncode, 0)


if __name__ == "__main__":
    unittest.main()
