#!/usr/bin/env python3
"""Publish only complete releases; never rewrite already-published versions."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

REPO = "so2liu/dauerbot"
ROOT = Path(__file__).resolve().parent.parent


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    tag = parser.parse_args().tag
    manifest = json.loads((ROOT / "dist/release/update.json").read_text())
    if tag != "v" + manifest["version"]:
        raise ValueError("Tag and signed manifest differ")
    directory = ROOT / "dist/release"
    names = [manifest["file"], "SHA256SUMS", "update.json", "update.json.sig", "signing.txt"]
    existing = subprocess.run(["gh", "release", "view", tag, "-R", REPO, "--json", "isDraft"],
                              capture_output=True, text=True)
    if existing.returncode == 0:
        if not json.loads(existing.stdout)["isDraft"]:
            raise ValueError("This version is already public; create a new version instead of overwriting it")
    else:
        gh("release", "create", tag, "-R", REPO, "--draft", "--verify-tag",
           "--title", f"Dauerbot {manifest['version']}", "--notes-file", str(directory / "release-notes.md"))
    gh("release", "upload", tag, "-R", REPO, "--clobber", *(str(directory / name) for name in names))
    # GitHub's tag endpoint does not resolve draft releases. gh resolves the
    # draft first, and its API URL addresses the release by its numeric ID.
    api_url = json.loads(gh("release", "view", tag, "-R", REPO, "--json", "apiUrl"))["apiUrl"]
    release = json.loads(gh("api", api_url))
    assets = {asset["name"]: asset for asset in release["assets"]}
    for name in names:
        expected = "sha256:" + hashlib.sha256((directory / name).read_bytes()).hexdigest()
        if name not in assets or assets[name].get("digest") != expected:
            raise ValueError(f"Uploaded asset is incomplete or checksum differs: {name}")
    # Local builds are private staging inputs, not installable release assets.
    for name in ("prebuilt.json", f"Dauerbot-{tag}-macos-arm64-unsigned.zip"):
        if name in assets:
            gh("release", "delete-asset", tag, name, "-R", REPO, "--yes")
    prerelease = "-" in manifest["version"]
    gh("release", "edit", tag, "-R", REPO, "--draft=false",
       f"--prerelease={str(prerelease).lower()}", "--latest=false",
       "--notes-file", str(directory / "release-notes.md"))
    # The workflow is serialized. A delayed old release cannot take Latest from a newer one.
    releases = json.loads(gh("api", f"repos/{REPO}/releases?per_page=100"))
    stable = []
    for release in releases:
        match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", release["tag_name"])
        if match and not release["draft"] and not release["prerelease"]:
            stable.append((tuple(map(int, match.groups())), release["tag_name"]))
    if stable:
        gh("release", "edit", max(stable)[1], "-R", REPO, "--latest")
    print(f"Published https://github.com/{REPO}/releases/tag/{tag}")


if __name__ == "__main__":
    main()
