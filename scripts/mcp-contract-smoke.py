#!/usr/bin/env python3
"""Keep an actual published 0.4.7 stdio MCP across a compatible service upgrade."""
import argparse
import hashlib
import io
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
    class ContractHandler(Handler):
        def do_POST(self):
            if self.path.startswith("/open-apis/im/v1/messages"):
                data = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                if b"contract-uncertain" in data:
                    FIXTURE.messages.append(json.loads(data))
                    self.close_connection = True  # Accepted remotely; receipt lost.
                    return
                original_input = self.rfile
                self.rfile = io.BytesIO(data)
                try:
                    return super().do_POST()
                finally:
                    self.rfile = original_input
            return super().do_POST()

    FIXTURE.server = ThreadingHTTPServer(("127.0.0.1", 0), ContractHandler)
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
                                             "{bridge_exe}", "{credentials}", "{native_session}", "--native-hook-fixture"]})
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
        subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", "-R",
                        '=anchor apple generic and certificate leaf[subject.OU] = "366ADQ28F6" and identifier "dev.herdr.feishu-bridge"',
                        str(old_app)], check=True, capture_output=True)
        assert subprocess.check_output([str(executable), "--version"], text=True).strip().endswith("0.4.7")
        daemon = boot()
        config = wait_for(lambda: json.loads((data / "config.json").read_text()) if (data / "config.json").exists() else None, "published daemon did not initialize")
        state = wait_for(lambda: api("/state"), "published daemon did not start")
        assert state["version"] == "0.4.7" and state["api_protocol"] == 7
        key, workspace = add("published-live", "cli_fixture")
        original = project(key)
        launch = launches(workspace)[0]
        assert original["native_session"] and not launch["legacy"]
        credentials = data / "sessions" / key / original["generation"] / "credentials.json"
        credential_bytes = credentials.read_bytes()
        assert "mcp_contract" not in json.loads(credential_bytes)
        companion = Path(launch["command"])
        companion_hash = hashlib.sha256(companion.read_bytes()).hexdigest()
        assert companion_hash == hashlib.sha256((old_app / "Contents/MacOS/hfb").read_bytes()).hexdigest()
        unrelated = legacy_rpc("workspace.create", {"cwd": str(run), "focus": False})["workspace"]["workspace_id"]
        unrelated_before = pane_identities(unrelated)
        inject(key, "contract-exercise-before")
        wait_for(lambda: (workspace / "contract-results.jsonl").exists(), "published three-tool exercise failed")
        inject(key, "contract-uncertain")
        wait_for(lambda: any(m["status"] == "uncertain" and m["source_id"] == "contract-uncertain" for m in api(f"/projects/{key}")["messages"]), "lost outbound receipt was not uncertain")
        inject(key, "hold-before-upgrade")
        wait_for(lambda: any(r["body"] == "hold-before-upgrade" for r in received(workspace)), "live work not observed")
        api(f"/projects/{key}/pause", {"paused": True})
        queued = inject(key, "queued-through-upgrade")
        identity = api("/state")
        (data / "updates").mkdir(exist_ok=True)
        (data / "updates/service-identity.json").write_text(json.dumps(identity))
        api("/service/shutdown", {})
        daemon.wait(timeout=20)
        # Replace only the fixture's mutable source; the original live stdio
        # process and retained immutable executable are deliberately untouched.
        replacement = executable.with_name("hfb.next")
        shutil.copy2(binary, replacement)
        replacement.replace(executable)
        daemon = boot()
        state = wait_for(lambda: (s if "mcp_contracts" in (s := api("/state")) else None), "contract-aware daemon did not start")
        assert state["api_protocol"] == 7 and state["mcp_contracts"] == [1]
        current = project(key)
        for field in ("generation", "terminal", "native_session", "execution"):
            assert current[field] == original[field], (field, current[field], original[field])
        assert credentials.read_bytes() == credential_bytes
        assert len(launches(workspace)) == 1
        os.kill(launch["mcp_pid"], 0)
        assert hashlib.sha256(companion.read_bytes()).hexdigest() == companion_hash
        assert pane_identities(unrelated) == unrelated_before
        assert not any(r["body"] == "queued-through-upgrade" for r in received(workspace))
        checks.append("Actual signed published 0.4.7 stdio MCP PID and private native Agent fixture, generation, credentials and immutable companion survive service replacement without stop/resume")
        api(f"/projects/{key}/pause", {"paused": False})
        wait_for(lambda: any("queued-through-upgrade" in m["content"] for m in FIXTURE.messages), "queued steer did not resume")
        inject(key, "contract-exercise-after")
        wait_for(lambda: len((workspace / "contract-results.jsonl").read_text().splitlines()) == 2, "old running stdio three tools failed after replacement")
        assert len(launches(workspace)) == 1
        assert sum(m.get("msg_type") == "file" for m in FIXTURE.messages) == 1
        assert sum("contract-text" in m.get("content", "") for m in FIXTURE.messages) == 1
        timers = api(f"/projects/{key}/timers")
        assert len(timers) == 1 and timers[0]["name"] == "Contract timer", timers
        checks.append("All three original tools work after replacement; repeated send_md/send_file/schedule_run keep exactly one effect with original request IDs")
        inject(key, "contract-restart")
        wait_for(lambda: len(launches(workspace)) == 2, "exact published companion startup probe/registration failed")
        restarted = launches(workspace)[1]
        assert restarted["pid"] == launch["pid"] and restarted["mcp_pid"] != launch["mcp_pid"]
        assert restarted["command"] == launch["command"] and restarted["generation"] == launch["generation"]
        wait_for(lambda: any("contract-restart" in m["content"] for m in FIXTURE.messages), "fresh original MCP could not send")
        checks.append("Exact old signed companion also restarts successfully through its unmodified API7 state probe and private native registration; Agent/native identity is unchanged")
        assert sum("queued-through-upgrade" in m.get("content", "") for m in FIXTURE.messages) == 1
        assert any(m["status"] == "uncertain" and m["source_id"] == "contract-uncertain" for m in api(f"/projects/{key}")["messages"])
        assert sum("contract-uncertain" in m.get("content", "") for m in FIXTURE.messages) == 1
        assert sum(r["body"] == "hold-before-upgrade" for r in received(workspace)) == 1
        checks.append("Queued input delivered once; lost outbound receipt remains uncertain without retry, prior submitted work is not replayed, and unrelated native pane is preserved")
        # A deliberately different engine/public API response proves the modern
        # client negotiates the stable wire identity, without a historical probe.
        modern_requests = []
        class ModernEngine(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                modern_requests.append((self.path, body))
                assert self.headers["Authorization"] == "Bearer contract-token"
                assert self.headers["x-hfb-generation"] == "contract-generation"
                assert body["mcp_contract"] == 1
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"ok": True, "api_protocol": 999, "service_api_protocol":999, "mcp_contract":1}).encode())

        modern_server = ThreadingHTTPServer(("127.0.0.1", 0), ModernEngine)
        threading.Thread(target=modern_server.serve_forever, daemon=True).start()
        modern_credentials = run / "modern/sessions/project/contract-generation/credentials.json"
        modern_credentials.parent.mkdir(parents=True)
        modern_credentials.write_text(json.dumps({"url":f"http://127.0.0.1:{modern_server.server_port}", "project":"project",
                                                 "token":"contract-token", "generation":"contract-generation", "mcp_contract":1}))
        modern = subprocess.Popen([str(binary), "mcp", "--credentials", str(modern_credentials)],
                                  env={**env,"HERDR_PANE_ID":"fixture-pane"}, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
        try:
            requests = [{"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"contract","version":"1"}}},
                        {"jsonrpc":"2.0", "method":"notifications/initialized"},
                        {"jsonrpc":"2.0", "id":2, "method":"tools/list"}]
            for request in requests:
                modern.stdin.write(json.dumps(request) + "\n")
            modern.stdin.flush()
            first = json.loads(modern.stdout.readline())
            second = json.loads(modern.stdout.readline())
            assert first["id"] == 1 and second["id"] == 2
            assert {tool["name"] for tool in second["result"]["tools"]} == {"send_md","send_file","schedule_run"}
            assert len(modern_requests) == 1 and modern_requests[0][0].endswith("/register")
        finally:
            modern.terminate()
            modern.wait(timeout=5)
            modern_server.shutdown()
        checks.append("Modern explicit contract1 stdio starts against a differing engine API999 registration with no /state probe or engine identity coupling")
        evidence = {"passed": True, "checks": checks, "run_dir": str(run), "published_version": "0.4.7", "public_api": 7,
                    "old_companion_sha256": companion_hash, "candidate_binary_sha256":hashlib.sha256(executable.read_bytes()).hexdigest(), "before": launch, "after_mcp_restart": restarted,
                    "queued_message": queued["id"], "native_session": original["native_session"],
                    "limitations":["Native hook identity is simulated in a private HERDR session, not a real model resume test", "This fixture tests contract survival across a bounded daemon replacement; listener/owner handover is covered separately"]}
        (results / "mcp-contract.json").write_text(json.dumps(evidence, indent=2) + "\n")
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
