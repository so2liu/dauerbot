#!/usr/bin/env python3
"""Notarize a Developer ID signed app and verify the distributable bundle."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def run(*command, timeout=600):
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{command[0]} timed out after {timeout} seconds") from None
    if result.returncode:
        message = result.stderr or result.stdout
        for name in ("APPLE_ID", "APPLE_APP_SPECIFIC_PASSWORD"):
            value = os.environ.get(name)
            if value:
                message = message.replace(value, "***")
        raise RuntimeError(f"{command[0]} failed ({result.returncode}): {message}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--app", type=Path, default=ROOT / "dist/Dauerbot.app")
    parser.add_argument("--keychain", type=Path, required=True)
    args = parser.parse_args()
    app = args.app.resolve()
    keychain = str(args.keychain.resolve())
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    run("codesign", "--verify", "--deep", "--strict", "-R",
        '=anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists'
        ' and certificate leaf[subject.OU] = "366ADQ28F6"'
        ' and identifier "dev.herdr.feishu-bridge"', str(app))
    run("xcrun", "notarytool", "store-credentials", "dauerbot-notary",
        "--apple-id", os.environ["APPLE_ID"],
        "--password", os.environ["APPLE_APP_SPECIFIC_PASSWORD"],
        "--team-id", os.environ["APPLE_TEAM_ID"], "--keychain", keychain)
    auth = ("--keychain-profile", "dauerbot-notary", "--keychain", keychain)
    with tempfile.TemporaryDirectory(prefix="dauerbot-notary-") as directory:
        archive = str(Path(directory) / "Dauerbot.zip")
        run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), archive)
        submission = json.loads(run("xcrun", "notarytool", "submit", archive,
                                    *auth, "--output-format", "json"))
        submission_id = submission["id"]
        report = output / "notarization.json"
        report.write_text(json.dumps(submission, indent=2) + "\n")
        print(f"Submitted to Apple: {submission_id}", flush=True)
        # Save the ID before waiting so a delayed submission can be inspected
        # without uploading the same binary again.
        wait_error = None
        try:
            run("xcrun", "notarytool", "wait", submission_id, *auth,
                "--timeout", "65m", timeout=4000)
        except RuntimeError as error:
            wait_error = error
        result = json.loads(run("xcrun", "notarytool", "info", submission_id,
                                *auth, "--output-format", "json"))
        report.write_text(json.dumps(result, indent=2) + "\n")
        if result["status"] != "Accepted":
            log = run("xcrun", "notarytool", "log", submission_id, *auth)
            (output / "notarization-log.json").write_text(log)
            raise RuntimeError(
                f"Apple notarization {result['status']}; see notarization-log.json"
            ) from wait_error
    run("xcrun", "stapler", "staple", str(app))
    run("xcrun", "stapler", "validate", str(app))
    run("codesign", "--verify", "--deep", "--strict", str(app))
    run("spctl", "--assess", "--type", "execute", "--verbose=4", str(app))
    print(f"Apple accepted {submission_id}; ticket stapled and Gatekeeper assessment passed.")


if __name__ == "__main__":
    main()
