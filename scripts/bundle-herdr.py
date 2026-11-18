#!/usr/bin/env python3
"""Fetch the pinned upstream payload at build time, never at application startup."""
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
VERSION = "0.9.5"
SHA256 = "fb77c2232a05a8035421382c894392f4174e4e38a18e48af3dd0290157c3a7e2"
SIZE = 22536176
URL = f"https://github.com/herdrdev/herdr/releases/download/v{VERSION}/herdr-macos-aarch64"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest(bundle):
    payload = bundle / "Contents/MacOS/herdr"
    return {"version": VERSION, "upstream_sha256": SHA256,
            "sha256": digest(payload), "size": payload.stat().st_size}


def verify(bundle):
    metadata = json.loads((bundle / "Contents/Resources/herdr-runtime.json").read_text())
    if metadata != manifest(bundle):
        raise ValueError("Bundled HERDR metadata differs from its signed payload")
    payload = bundle / "Contents/MacOS/herdr"
    if payload.read_bytes()[:8] != bytes.fromhex("cffaedfe0c000001"):
        raise ValueError("Bundled HERDR requires Apple Silicon")
    machine = platform.machine().lower()
    if machine in ("arm64", "aarch64"):
        # Mandatory local ARM staging executes the pinned runtime. Intel CI
        # only signs this already verified ARM package; it cannot execute it.
        if subprocess.check_output([str(payload), "--version"], text=True, timeout=5).strip() != "herdr 0.9.5":
            raise ValueError("Unsupported bundled HERDR version")
    elif sys.platform != "darwin" or machine not in ("x86_64", "amd64"):
        raise ValueError("Bundled HERDR verification requires a supported macOS host")
    for name in ("LICENSE", "ATTRIBUTION.txt", "libghostty-vt-LICENSE", "portable-pty-LICENSE"):
        if (bundle / "Contents/Resources/herdr" / name).read_bytes() != (ROOT / "packaging/herdr" / name).read_bytes():
            raise ValueError("Missing or changed HERDR attribution: " + name)
    subprocess.run(["codesign", "--verify", "--strict", str(payload)], check=True)


def main():
    command, app = sys.argv[1:]
    bundle = Path(app).resolve()
    if command == "fetch":
        cache = ROOT / ".local/herdr/0.9.5"
        cache.mkdir(parents=True, exist_ok=True)
        cached = cache / "herdr-macos-aarch64"
        if not cached.exists() or cached.stat().st_size != SIZE or digest(cached) != SHA256:
            with tempfile.TemporaryDirectory(dir=cache) as directory:
                download = Path(directory) / "herdr"
                subprocess.run(["curl", "--fail", "--location", "--retry", "3", "--output", str(download), URL], check=True)
                if download.stat().st_size != SIZE or digest(download) != SHA256:
                    raise ValueError("Official HERDR payload size or SHA-256 differs")
                download.replace(cached)
        destination = bundle / "Contents/MacOS/herdr"
        shutil.copyfile(cached, destination)
        destination.chmod(0o755)
        shutil.copytree(ROOT / "packaging/herdr", bundle / "Contents/Resources/herdr", dirs_exist_ok=True)
    elif command == "manifest":
        (bundle / "Contents/Resources/herdr-runtime.json").write_text(json.dumps(manifest(bundle), indent=2) + "\n")
    elif command == "verify":
        verify(bundle)
    else:
        raise ValueError("Unknown bundle operation")


if __name__ == "__main__":
    main()
