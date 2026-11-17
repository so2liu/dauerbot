#!/usr/bin/env python3
"""Isolated real cross-version daemon/bundle/MCP upgrade regression.

Requires an extracted, published Developer ID signed 0.4.4 app and a new hfb.
No real app installation, user configuration, native account, or database patches.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import ThreadingHTTPServer

from smoke import FIXTURE, Handler, wait_for

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--old-app", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    old_app = args.old_app.resolve()
    results = ROOT / "test-results"
    results.mkdir(exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="db-up-", dir="/tmp")).resolve()
    home = run / "home"
    home.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("HERDR_", "HC_")) and k not in
           ("SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY", "SSH_ORIGINAL_COMMAND")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(run / "xdg"), XDG_STATE_HOME=str(run / "state"), SHELL="/bin/sh")
    herdr = str(Path(shutil.which("herdr")).resolve())
    session = "upgrade-" + run.name
    FIXTURE.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=FIXTURE.server.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    data = run / "data"
    app = run / "Dauerbot.app"
    shutil.copytree(old_app, app)
    executable = app / "Contents/MacOS/hfb"
    log = (run / "daemon.log").open("w")
    daemon = None
    checks = []

    def boot():
        return subprocess.Popen([str(executable), "--data-dir", str(data), "serve", "--port", str(port),
                                 "--herdr", herdr, "--herdr-session", session,
                                 "--feishu-base", f"http://127.0.0.1:{FIXTURE.server.server_port}"],
                                env=env, stdout=log, stderr=log)

    config = None

    def api(path, body=None, method=None):
        request = urllib.request.Request(f"http://127.0.0.1:{port}/api{path}",
                                        data=None if body is None else json.dumps(body).encode(),
                                        method=method or ("POST" if body is not None else "GET"),
                                        headers={"Authorization": "Bearer " + config["token"], "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=40) as response:
            return json.load(response)

    def project(key):
        return api(f"/projects/{key}")["project"]

    def legacy_rpc(method, params=None):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(10)
            conn.connect(str(run / "xdg/herdr/sessions" / session / "herdr.sock"))
            conn.sendall((json.dumps({"id": "upgrade", "method": method, "params": params or {}}) + "\n").encode())
            response = json.loads(conn.makefile().readline())
            assert "error" not in response, response
            return response["result"]

    def pane_identities(workspace_id):
        # Focus, title and output counters may change while other workspaces open.
        return [(p["pane_id"], p["terminal_id"], p["cwd"])
                for p in legacy_rpc("pane.list", {"workspace_id": workspace_id})["panes"]]

    def add(name, bot):
        workspace = run / name
        workspace.mkdir()
        p = api("/projects", {"name": name, "path": str(workspace), "agent": "custom",
                              "extra_args": [sys.executable, str(ROOT / "scripts/upgrade_agent.py"),
                                             "{bridge_exe}", "{credentials}", "{native_session}"]})
        api(f"/projects/{p['id']}/bot", {"app_id": bot, "app_secret": "fixture-secret"}, "PUT")
        api(f"/projects/{p['id']}/start", {})
        wait_for(lambda: project(p["id"])["ready"], f"{name} MCP not ready", timeout=40)
        wait_for(lambda: (workspace / "launches.jsonl").exists(), f"{name} MCP tools not registered")
        return p["id"], workspace

    def inject(key, body):
        return api(f"/projects/{key}/messages", {"content": body})

    def received(workspace):
        path = workspace / "received.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def launches(workspace):
        return [json.loads(line) for line in (workspace / "launches.jsonl").read_text().splitlines()]

    try:
        daemon = boot()
        config = wait_for(lambda: json.loads((data / "config.json").read_text()) if (data / "config.json").exists() else None, "old daemon did not initialize")
        state = wait_for(lambda: api("/state"), "old daemon did not start")
        assert state["version"] == "0.4.4" and state["api_protocol"] == 4, state
        first, first_dir = add("already-live", "cli_fixture")
        first_native = project(first)["native_session"]
        assert first_native, "Historical fixture never established native continuity"
        old_launch = launches(first_dir)[0]
        inject(first, "before-swap")
        wait_for(lambda: any("before-swap" in m["content"] for m in FIXTURE.messages), "old MCP did not send")
        unrelated = legacy_rpc("workspace.create", {"cwd": str(run), "focus": False})["workspace"]["workspace_id"]
        unrelated_before = pane_identities(unrelated)
        stage = run / ".dauerbot-update-fixture"
        stage.mkdir()
        backup = stage / "old"
        app.rename(backup)
        executable.parent.mkdir(parents=True)
        shutil.copy2(binary, executable)
        (data / "updates").mkdir(exist_ok=True)
        (data / "updates/installed.json").write_text(json.dumps({"app": str(app), "backup": str(backup), "version": "0.4.6"}))
        second, second_dir = add("after-swap", "cli_second")
        assert launches(second_dir)[0]["legacy"], "The old daemon did not receive its original MCP contract"
        pinned_old = list((data / "runtime/hfb/0.4.4-api4").glob("*/hfb"))
        assert len(pinned_old) == 1
        assert hashlib.sha256(pinned_old[0].read_bytes()).digest() == hashlib.sha256((backup / "Contents/MacOS/hfb").read_bytes()).digest()
        inject(second, "after-swap")
        wait_for(lambda: any("after-swap" in m["content"] for m in FIXTURE.messages), "historical fallback did not deliver")
        os.kill(old_launch["mcp_pid"], 0)
        checks.append("Published 0.4.4/API4 daemon survives bundle swap; newly spawned replacement hfb execs signed original MCP; old live MCP remains running")
        # A later updater may remove stage/old. Retained historical bytes still work.
        backup.rename(stage / "retained-test-backup")
        old_cache = data / "runtime/hfb/0.4.4-api4"
        saved_cache = data / "runtime/hfb/saved-test-cache"
        old_cache.rename(saved_cache)
        missing_dir = run / "missing-backup"
        missing_dir.mkdir()
        missing = api("/projects", {"name": "missing-backup", "path": str(missing_dir), "agent": "custom",
                                    "extra_args": [sys.executable, str(ROOT / "scripts/upgrade_agent.py"),
                                                   "{bridge_exe}", "{credentials}", "{native_session}"]})
        api(f"/projects/{missing['id']}/start", {})
        wait_for(lambda: (json.loads((data / "updates/status.json").read_text())["phase"] == "update_required")
                 if (data / "updates/status.json").exists() else False, "missing historical backup did not report update_required")
        assert not project(missing["id"])["ready"]
        assert not (missing_dir / "received.jsonl").exists()
        saved_cache.rename(old_cache)
        checks.append("Missing signed backup and retained companion fail closed with actionable update_required; no input effects are replayed")
        inject(first, "hold-before-upgrade")
        wait_for(lambda: any(r["body"] == "hold-before-upgrade" for r in received(first_dir)), "old live Agent did not receive held work")
        api(f"/projects/{first}/pause", {"paused": True})
        queued = inject(first, "queued-during-upgrade")
        old_state = api("/state")
        # This is the exact authenticated state captured by desktop ensure_service;
        # production inherit_service_protocol is exercised on the next real boot.
        (data / "updates/service-identity.json").write_text(json.dumps(old_state))
        api("/service/shutdown", {})
        daemon.wait(timeout=20)
        daemon = boot()
        state = wait_for(lambda: (s if (s := api("/state"))["api_protocol"] == 6 else None), "new daemon did not start")
        for key in (first, second):
            assert next(p for p in state["projects"] if p["id"] == key)["lifecycle"] == "update_required"
        os.kill(old_launch["mcp_pid"], 0)
        assert project(first)["native_session"] == first_native
        assert not any(r["body"] == "queued-during-upgrade" for r in received(first_dir))
        assert pane_identities(unrelated) == unrelated_before
        work = api(f"/projects/{first}/work")
        assert any(r["status"] == "submitted" for r in work["runs"]), work
        checks.append("Active old MCP survives service replacement but its generation becomes update_required; queued input is held and active work remains recorded until explicit recovery")
        api(f"/projects/{first}/recover-after-update", {})
        wait_for(lambda: project(first)["ready"] and project(first)["generation"] != old_launch["generation"], "explicit recovery did not reconnect tools")
        fresh = wait_for(lambda: (ls[-1] if len(ls := launches(first_dir)) == 2 else None), "no recovered native Agent")
        assert fresh["native"] == first_native and fresh["resume"] == first_native and not fresh["legacy"], fresh
        assert any(r["status"] == "uncertain" for r in api(f"/projects/{first}/work")["runs"])
        api(f"/projects/{first}/pause", {"paused": False})
        wait_for(lambda: any("queued-during-upgrade" in m["content"] for m in FIXTURE.messages), "saved input did not continue automatically")
        assert sum(r["body"] == "queued-during-upgrade" for r in received(first_dir)) == 1
        assert sum(r["body"] == "hold-before-upgrade" for r in received(first_dir)) == 1
        assert api(f"/projects/{first}")["messages"]
        assert pane_identities(unrelated) == unrelated_before
        checks.append("One explicit recovery action replaces only the owned incompatible pane, resumes the saved native identity, registers three tools and delivers queued input once without replaying uncertain work")
        live = project(first)
        active = launches(first_dir)[-1]
        pinned_command = Path(active["command"])
        assert pinned_command.is_relative_to(data / "runtime/hfb/0.4.6-api6")
        before_hash = hashlib.sha256(pinned_command.read_bytes()).hexdigest()
        # Replace the original source and restart only the daemon on the same protocol.
        replacement = executable.with_name("hfb.next")
        shutil.copy2(binary, replacement)
        replacement.replace(executable)
        state = api("/state")
        (data / "updates/service-identity.json").write_text(json.dumps(state))
        api("/service/shutdown", {})
        daemon.wait(timeout=20)
        daemon = boot()
        wait_for(lambda: api("/state")["api_protocol"] == 6, "compatible daemon did not start")
        assert project(first)["generation"] == live["generation"]
        os.kill(active["mcp_pid"], 0)
        inject(first, "compatible-live-reuse")
        wait_for(lambda: any("compatible-live-reuse" in m["content"] for m in FIXTURE.messages), "live compatible MCP stopped working")
        assert len(launches(first_dir)) == 2
        assert hashlib.sha256(pinned_command.read_bytes()).hexdigest() == before_hash
        checks.append("Same-protocol daemon restart and source replacement reuse the same live Agent/MCP generation and immutable bytes")
        evidence = {"passed": True, "checks": checks, "run_dir": str(run), "old_version": "0.4.4", "old_protocol": 4, "new_protocol": 6, "queued_message": queued["id"]}
        (results / "upgrade-companion.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2))
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=20)
        if (run / "xdg/herdr/sessions" / session / "herdr.sock").exists():
            subprocess.run([herdr, "--session", session, "server", "stop"], env=env, capture_output=True, timeout=15)
        FIXTURE.server.shutdown()
        log.close()
        print(f"Isolated fixture evidence: {run}")


if __name__ == "__main__":
    main()
