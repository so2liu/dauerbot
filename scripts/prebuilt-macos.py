#!/usr/bin/env python3
"""Stage a locally verified build; restore it in CI before Apple signing."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import plistlib
import stat
import subprocess
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
REPO = "so2liu/dauerbot"


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def verify_app(version):
    app = ROOT / "dist/Dauerbot.app"
    with (app / "Contents/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    if (info["CFBundleShortVersionString"], info["CFBundleIdentifier"]) != (
        version, "dev.herdr.feishu-bridge"
    ):
        raise ValueError("Prebuilt bundle version or identifier differs")
    for name in ("hfb", "HerdrBridge"):
        with (app / "Contents/MacOS" / name).open("rb") as source:
            if source.read(8) != bytes.fromhex("cffaedfe0c000001"):
                raise ValueError("Prebuilt release requires Apple Silicon binaries")
    run("codesign", "--verify", "--deep", "--strict", str(app))


def draft(tag):
    result = subprocess.run(
        ["gh", "release", "view", tag, "-R", REPO, "--json", "isDraft,assets"],
        capture_output=True, text=True, cwd=ROOT,
    )
    if result.returncode:
        if "release not found" in result.stderr.lower() or "404" in result.stderr:
            return None
        raise RuntimeError("Cannot inspect staged release: " + result.stderr)
    value = json.loads(result.stdout)
    if not value["isDraft"]:
        raise ValueError("This release is already public; use a new version")
    return value


def stage(tag, version):
    if run("git", "status", "--porcelain"):
        raise ValueError("Commit source changes before staging a release")
    run("git", "fetch", "origin", "main")
    tree = run("git", "rev-parse", "HEAD^{tree}")
    if tree != run("git", "rev-parse", "origin/main^{tree}"):
        raise ValueError("Local source must match main before staging a release")
    for name in ("Cargo.toml", "desktop/Cargo.toml"):
        with (ROOT / name).open("rb") as source:
            if tomllib.load(source)["package"]["version"] != version:
                raise ValueError(f"{name}: package version differs from tag")
    existing = draft(tag)
    for command in (
        ("./scripts/build-desktop.sh",),
        ("cargo", "test", "--locked", "--release", "--target-dir", "desktop/target", "--tests"),
        ("cargo", "test", "--locked", "--release", "--manifest-path", "desktop/Cargo.toml"),
        ("cargo", "clippy", "--locked", "--release", "--target-dir", "desktop/target", "--all-targets", "--", "-D", "warnings"),
        ("cargo", "clippy", "--locked", "--release", "--manifest-path", "desktop/Cargo.toml", "--all-targets", "--", "-D", "warnings"),
    ):
        subprocess.run(command, cwd=ROOT, check=True)
    verify_app(version)
    output = ROOT / "dist/prebuilt"
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"Dauerbot-{tag}-macos-arm64-unsigned.zip"
    run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent",
        str(ROOT / "dist/Dauerbot.app"), str(archive))
    manifest = output / "prebuilt.json"
    manifest.write_text(json.dumps({
        "version": version, "source_commit": run("git", "rev-parse", "origin/main"),
        "source_tree": tree, "file": archive.name, "size": archive.stat().st_size,
        "sha256": digest(archive),
    }, indent=2) + "\n")
    if existing is None:
        run("gh", "release", "create", tag, "-R", REPO, "--draft",
            "--target", run("git", "rev-parse", "origin/main"),
            "--title", f"Dauerbot {version}", "--notes", "Local build staged for signing and notarization.")
    run("gh", "release", "upload", tag, "-R", REPO, "--clobber", str(archive), str(manifest))
    print(f"Staged verified local build for {tag}. Push the main-branch tag to sign, notarize, and publish.")


def restore(tag, version):
    release = draft(tag)
    names = {asset["name"] for asset in release["assets"]} if release else set()
    available = "prebuilt.json" in names
    if available:
        archive_name = f"Dauerbot-{tag}-macos-arm64-unsigned.zip"
        with tempfile.TemporaryDirectory(prefix="dauerbot-prebuilt-") as directory:
            run("gh", "release", "download", tag, "-R", REPO, "--dir", directory,
                "--pattern", "prebuilt.json", "--pattern", archive_name)
            manifest = json.loads((Path(directory) / "prebuilt.json").read_text())
            if (manifest["version"], manifest["source_tree"], manifest["file"]) != (
                version, run("git", "rev-parse", "HEAD^{tree}"), archive_name
            ):
                raise ValueError("Staged build does not match the release source tree")
            archive = Path(directory) / archive_name
            if (archive.stat().st_size > 128 * 1024 * 1024
                    or archive.stat().st_size != manifest["size"]
                    or digest(archive) != manifest["sha256"]):
                raise ValueError("Staged build size or checksum differs")
            with zipfile.ZipFile(archive) as bundle:
                if sum(entry.file_size for entry in bundle.infolist()) > 512 * 1024 * 1024:
                    raise ValueError("Staged build is too large")
                for entry in bundle.infolist():
                    path = PurePosixPath(entry.filename)
                    if (path.is_absolute() or ".." in path.parts
                            or not path.parts or path.parts[0] not in ("Dauerbot.app", "__MACOSX")
                            or stat.S_ISLNK(entry.external_attr >> 16)):
                        raise ValueError("Unsafe staged archive entry")
            (ROOT / "dist").mkdir(exist_ok=True)
            run("ditto", "-x", "-k", str(archive), str(ROOT / "dist"))
            verify_app(version)
        print("Restored verified local build; CI will sign and notarize without recompiling.")
    else:
        print("No local build staged; CI will compile and test from source.")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"available={str(available).lower()}\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("stage", "restore"))
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()
    version = args.tag.removeprefix("v")
    with (ROOT / "Cargo.toml").open("rb") as source:
        if args.tag != "v" + tomllib.load(source)["package"]["version"]:
            raise ValueError("Tag must match the package version")
    (stage if args.command == "stage" else restore)(args.tag, version)


if __name__ == "__main__":
    main()
