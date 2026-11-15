#!/usr/bin/env bash
# Usage: sign-macos.sh /path/to/Dauerbot.app IDENTITY [KEYCHAIN]
# "-" produces a local ad-hoc signature; certificates remain in the keychain.
set -euo pipefail
bundle="${1:?Specify the .app path}"
identity="${2:?Specify a signing identity, or - for an ad-hoc signature}"
args=(--force --sign "$identity")
if [[ -n "${3:-}" ]]; then args+=(--keychain "$3"); fi
if [[ "$identity" != "-" ]]; then args+=(--timestamp --options runtime); fi
codesign "${args[@]}" "$bundle/Contents/MacOS/hfb"
codesign "${args[@]}" "$bundle"
codesign --verify --deep --strict "$bundle"
codesign --display --verbose=2 "$bundle"
