#!/usr/bin/env bash
# Usage: sign-macos.sh /path/to/Dauerbot.app IDENTITY [KEYCHAIN]
# "-" produces a local ad-hoc signature; certificates remain in the keychain.
set -euo pipefail
bundle="${1:?需要 .app 路径}"
identity="${2:?需要签名身份，或使用 - 进行本地签名}"
args=(--force --sign "$identity")
if [[ -n "${3:-}" ]]; then args+=(--keychain "$3"); fi
if [[ "$identity" != "-" ]]; then args+=(--timestamp --options runtime); fi
codesign "${args[@]}" "$bundle/Contents/MacOS/hfb"
codesign "${args[@]}" "$bundle"
codesign --verify --deep --strict "$bundle"
codesign --display --verbose=2 "$bundle"
