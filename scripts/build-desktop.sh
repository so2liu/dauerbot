#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Keep existing CLI available beside the native application executable.
pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend build
cargo build --locked --release --bin hfb --target-dir desktop/target
cargo build --locked --manifest-path desktop/Cargo.toml --release
bundle="$PWD/dist/Herdr Bridge.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp packaging/Info.plist "$bundle/Contents/Info.plist"
cp desktop/target/release/HerdrBridge "$bundle/Contents/MacOS/HerdrBridge"
cp desktop/target/release/hfb "$bundle/Contents/MacOS/hfb"
if [[ -f packaging/AppIcon.icns ]]; then cp packaging/AppIcon.icns "$bundle/Contents/Resources/AppIcon.icns"; fi
codesign --force --sign - "$bundle/Contents/MacOS/hfb"
codesign --force --sign - "$bundle"
codesign --verify --deep --strict "$bundle"
printf '已构建：%s\n' "$bundle"
