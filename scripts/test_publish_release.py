"""Release publication gates, using only temporary files and a mocked gh client."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "publish_release", Path(__file__).with_name("publish-release.py")
)
publish = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publish)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="release-gate-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        directory = self.root / "dist/release"
        directory.mkdir(parents=True)
        manifest = {"version": "0.6.5", "file": "Dauerbot-v0.6.5-macos-arm64.zip"}
        (directory / "update.json").write_text(json.dumps(manifest))
        self.names = [manifest["file"], "SHA256SUMS", "update.json", "update.json.sig", "signing.txt"]
        for name in self.names:
            if name != "update.json":
                (directory / name).write_text("fixture signed asset: " + name)
        (directory / "release-notes.md").write_text("Verified fixture release notes")
        self.assets = [
            {"name": name, "digest": "sha256:" + hashlib.sha256((directory / name).read_bytes()).hexdigest()}
            for name in self.names
        ] + [{"name": name} for name in (
            "prebuilt.json", "Dauerbot-v0.6.5-macos-arm64-unsigned.zip"
        )]
        self.calls = []
        self.api_draft = True
        self.view = subprocess.CompletedProcess([], 0, '{"isDraft":true}', "")

    def gh(self, *arguments):
        self.calls.append(arguments)
        if arguments[:2] == ("release", "view"):
            return json.dumps({"apiUrl": "https://api.github.com/repos/fixture/releases/1"})
        if arguments == ("api", "https://api.github.com/repos/fixture/releases/1"):
            return json.dumps({"draft": self.api_draft, "assets": self.assets})
        if arguments == ("api", "repos/so2liu/dauerbot/releases?per_page=100"):
            return json.dumps([
                {"tag_name": "v0.6.5", "draft": False, "prerelease": False},
                {"tag_name": "v0.6.4", "draft": False, "prerelease": False},
                {"tag_name": "v0.7.0", "draft": True, "prerelease": False},
            ])
        return ""

    def run_publish(self, *options):
        with mock.patch.object(publish, "ROOT", self.root), \
                mock.patch.object(publish, "gh", side_effect=self.gh), \
                mock.patch.object(publish.subprocess, "run", return_value=self.view), \
                mock.patch("builtins.print"):
            publish.main(["--tag", "v0.6.5", *options])

    def test_signed_stage_verifies_assets_and_notes_without_publication_or_cleanup(self):
        self.run_publish("--stage-only")
        self.assertTrue(any(call[:2] == ("release", "upload") for call in self.calls))
        edits = [call for call in self.calls if call[:2] == ("release", "edit")]
        self.assertEqual(len(edits), 1)
        self.assertIn("--notes-file", edits[0])
        self.assertFalse(any(call[:2] in (("release", "create"), ("release", "delete-asset")) for call in self.calls))
        self.assertFalse(any(argument.startswith(("--draft", "--latest", "--prerelease")) for call in self.calls for argument in call))
        self.assertNotIn(("api", "repos/so2liu/dauerbot/releases?per_page=100"), self.calls)

    def test_default_publication_still_cleans_staging_publishes_and_promotes_latest(self):
        self.run_publish()
        deleted = [call[3] for call in self.calls if call[:2] == ("release", "delete-asset")]
        self.assertEqual(deleted, ["prebuilt.json", "Dauerbot-v0.6.5-macos-arm64-unsigned.zip"])
        publish_edit = next(call for call in self.calls if "--draft=false" in call)
        self.assertIn("--prerelease=false", publish_edit)
        self.assertIn("--latest=false", publish_edit)
        self.assertEqual(self.calls[-1], ("release", "edit", "v0.6.5", "-R", "so2liu/dauerbot", "--latest"))

    def test_signed_stage_requires_an_existing_private_draft(self):
        for view in [subprocess.CompletedProcess([], 1, "", "release not found"),
                     subprocess.CompletedProcess([], 0, '{"isDraft":false}', "")]:
            with self.subTest(view=view):
                self.calls.clear()
                self.view = view
                with self.assertRaises(ValueError):
                    self.run_publish("--stage-only")
                self.assertEqual(self.calls, [])

    def test_stage_refuses_digest_mismatch_before_notes_or_publication(self):
        self.assets[0]["digest"] = "sha256:incorrect"
        with self.assertRaisesRegex(ValueError, "checksum differs"):
            self.run_publish("--stage-only")
        self.assertFalse(any(call[:2] in (("release", "edit"), ("release", "delete-asset")) for call in self.calls))

    def test_stage_checks_that_api_release_remains_private(self):
        self.api_draft = False
        with self.assertRaisesRegex(ValueError, "remain private"):
            self.run_publish("--stage-only")
        self.assertFalse(any(call[:2] in (("release", "edit"), ("release", "delete-asset")) for call in self.calls))


if __name__ == "__main__":
    unittest.main()
