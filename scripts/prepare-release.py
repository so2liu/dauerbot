#!/usr/bin/env python3
"""Package and sign public update metadata. Private keys stay in CI environment."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import tomllib

from nacl.signing import SigningKey

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()
    version = args.tag.removeprefix("v")
    if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Invalid release tag")
    for name in ("Cargo.toml", "desktop/Cargo.toml"):
        with (ROOT / name).open("rb") as source:
            if tomllib.load(source)["package"]["version"] != version:
                raise ValueError(f"{name}: package version differs from tag")
    if json.loads((ROOT / "frontend/package.json").read_text())["version"] != version:
        raise ValueError("Frontend version differs from tag")
    app = ROOT / "dist/Dauerbot.app"
    with (app / "Contents/Info.plist").open("rb") as source:
        info = plistlib.load(source)
    if info["CFBundleShortVersionString"] != version:
        raise ValueError("App version differs from tag")
    if info["CFBundleIdentifier"] != "dev.herdr.feishu-bridge":
        raise ValueError("Unexpected bundle identifier")
    for name in ["HerdrBridge", "hfb"]:
        with (app / "Contents/MacOS" / name).open("rb") as source:
            if source.read(8) != bytes.fromhex("cffaedfe0c000001"):
                raise ValueError("Release requires native Apple Silicon binaries")
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    signing = subprocess.run(["codesign", "--display", "--verbose=4", str(app)],
                             check=True, capture_output=True, text=True).stderr
    team = re.search(r"^TeamIdentifier=(.+)$", signing, re.M)
    if not team or team[1] != "366ADQ28F6":
        raise ValueError("App is not signed by the expected team")
    key = SigningKey(base64.b64decode(os.environ["UPDATE_SIGNING_KEY"], validate=True))
    expected_key = (ROOT / "packaging/update-public-key.txt").read_text().strip()
    if base64.b64encode(bytes(key.verify_key)).decode() != expected_key:
        raise ValueError("CI signing key does not match the embedded update public key")
    output = ROOT / "dist/release"
    output.mkdir(parents=True, exist_ok=True)
    package = output / f"Dauerbot-v{version}-macos-arm64.zip"
    subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), str(package)], check=True)
    manifest = {
        "version": version,
        "bundle_id": info["CFBundleIdentifier"],
        "team_id": team[1],
        "architecture": "arm64",
        "file": package.name,
        "size": package.stat().st_size,
        "sha256": digest(package),
    }
    raw = (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode()
    (output / "update.json").write_bytes(raw)
    sig = key.sign(b"Dauerbot update v1\n" + raw).signature
    (output / "update.json.sig").write_text(base64.b64encode(sig).decode() + "\n")
    (output / "signing.txt").write_text(signing.replace(str(app), "Dauerbot.app") +
                                       "\nNotarization: not performed. Signed macOS preview.\n")
    public = [package, output / "update.json", output / "update.json.sig", output / "signing.txt"]
    (output / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.name}\n" for path in public))
    changelog = (ROOT / "CHANGELOG.md").read_text()
    section = re.search(rf"^## {re.escape(version)}[^\n]*\n(.*?)(?=^## |\Z)", changelog, re.M | re.S)
    notes = section[1].strip() if section else "详见仓库 CHANGELOG.md。"
    (output / "release-notes.md").write_text(
        f"Dauerbot {version}\n\n{notes}\n\n"
        "下载 Apple Silicon zip，解压后拖入 Applications。需先安装 HERDR Core（hc）和已登录的 Agent CLI。\n\n"
        "此安装包已签名，但尚未完成 Apple 公证。macOS 可能阻止直接打开；开发证书不等于 Developer ID 分发验收。\n\n"
        "已有自动升级版会校验更新清单签名、SHA-256、Apple Team 与 bundle ID；"
        "后台替换应用，下次打开生效。更新不结束 HERDR 中的主会话。\n"
    )
    print(f"Prepared {version}: {package.stat().st_size} bytes; signed update manifest.")


if __name__ == "__main__":
    main()
