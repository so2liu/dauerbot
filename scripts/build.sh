#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/.."
pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend check
pnpm --dir frontend build
cargo build --locked --release
