#!/usr/bin/env python3
"""Bounded native GPUI focus regression; never starts the Dauerbot daemon.

Build:
  cargo build --locked --manifest-path desktop/Cargo.toml --example focus_deadlock
Run:
  python3 scripts/focus-deadlock-smoke.py desktop/target/debug/examples/focus_deadlock

To demonstrate the original bug, build the same example against unpatched
GPUI 0.2.2 in a separate temporary checkout, then pass --expect-deadlock.
The watchdog samples and terminates only its own child process.
"""

import argparse
import pathlib
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument("--expect-deadlock", action="store_true")
    parser.add_argument("--sample", type=pathlib.Path)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires a macOS graphical login session")
    child = subprocess.Popen(
        [str(args.binary.resolve())],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        stdout, stderr = child.communicate(timeout=8)
    except subprocess.TimeoutExpired:
        sample_path = args.sample
        with tempfile.TemporaryDirectory(prefix="dauerbot-focus-sample-") as temporary:
            if sample_path is None:
                sample_path = pathlib.Path(temporary) / "sample.txt"
            try:
                subprocess.run(
                    ["sample", str(child.pid), "1", "-file", str(sample_path)],
                    capture_output=True,
                    timeout=5,
                    check=True,
                )
                sample = sample_path.read_text()
            finally:
                child.kill()
                stdout, stderr = child.communicate(timeout=3)
        print(stdout, end="")
        print(stderr, end="", file=sys.stderr)
        expected_stack = (
            "window_did_change_key_status" in sample
            and "resignKeyWindow" in sample
            and "lock_slow" in sample
        )
        if args.expect_deadlock and "FOCUS_REENTRANCY_BEGIN" in stdout and expected_stack:
            print("PASS: unpatched native focus callback deadlocked; watchdog reaped fixture")
            return
        raise SystemExit("FAIL: native focus fixture timed out")
    print(stdout, end="")
    print(stderr, end="", file=sys.stderr)
    if args.expect_deadlock:
        raise SystemExit("FAIL: expected the unpatched native deadlock")
    if child.returncode or "FOCUS_EVENT_LOOP_ALIVE" not in stdout:
        raise SystemExit(f"FAIL: fixture exited {child.returncode} without heartbeat")
    print("PASS: reentrant native focus returned and the event loop remains responsive")


if __name__ == "__main__":
    main()
