# GPUI 0.2.2

This directory contains the published `gpui` 0.2.2 crate, under its original
Apache-2.0 license in `LICENSE-APACHE`.

- Registry archive SHA-256: `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`.
- Published VCS metadata: `69e2130295c2649963eb639fc70b4f2ee8ea1624`,
  `crates/gpui` (the published crate reports a dirty tree).
- Omitted upstream example programs and their image/font assets, the corresponding
  normalized `Cargo.toml` example target declarations, and the crate's standalone
  lockfile. The original manifest is retained as `Cargo.toml.orig`.
- No upstream source files have been removed.

## Reentrant macOS key-window callback

Backport of [zed-industries/zed#51035](https://github.com/zed-industries/zed/pull/51035),
merge commit `97049a76baaf2299be73fafa06910ccf060e7589`.
Upstream now locates this callback in `gpui_macos`; 0.2.2 locates it in
`src/platform/mac/window.rs`.

Release the window-state mutex before calling AppKit's `resignKeyWindow`.
AppKit can synchronously call the same delegate again for `windowDidResignKey`;
holding the mutex across that call deadlocks the main thread.
The callback change is identical to the upstream patch.

The desktop root patches crates.io so direct GPUI users, `gpui-component`, and
`gpui-terminal` resolve to a single patched crate. Remove this vendor patch
when a compatible upstream release includes the fix.

Run `cargo build --locked --manifest-path desktop/Cargo.toml --example focus_deadlock`,
then `python3 scripts/focus-deadlock-smoke.py desktop/target/debug/examples/focus_deadlock`
from the repository root in a macOS graphical login session.
The example opens a native popup followed by a normal window and requires a
subsequent main-thread heartbeat. The watchdog limits execution to eight seconds;
with `--expect-deadlock`, it additionally requires a process sample containing the
reentrant key-window callback and mutex wait. It only terminates its own fixture.
