"""Isolated manager/provider → real legacy HERDR/MCP → Feishu protocol acceptance.

Uses only scripted local HTTP replies and private fixture processes; never a real
model account, native login or live Dauerbot data directory.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol_fixture", ROOT / "scripts/smoke.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Model:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.steps = []
        self.trigger = None
        self.started = threading.Event()
        self.release = threading.Event()

    def script(self, *steps, trigger=None):
        with self.lock:
            self.steps = list(steps)
            self.trigger = trigger
        self.started.clear()
        self.release.clear()

    def reply(self, body):
        with self.lock:
            self.requests.append(body)
            matched = self.trigger is None or self.trigger in json.dumps(body)
            step = self.steps.pop(0) if self.steps and matched else {"text": ""}
        if matched:
            self.started.set()
        if step.get("wait"):
            assert self.release.wait(20), "controlled provider request was not released"
        if step.get("error"):
            return 503, {"error": {"message": "Fixture provider unavailable", "type": "unavailable"}}
        message = {"role": "assistant", "content": step.get("text")}
        if "tool" in step:
            message["tool_calls"] = [{"id": step.get("id", "call_" + uuid.uuid4().hex), "type": "function", "function": {"name": step["tool"], "arguments": json.dumps(step["arguments"])}}]
        return 200, {"id": "chatcmpl_fixture", "object": "chat.completion", "created": 1, "model": "fixture-manager", "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls" if "tool" in step else "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}


MODEL = Model()


class ModelHandler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        assert self.headers.get("Authorization") == "Bearer manager-fixture-secret"
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body.get("stream") is not True, "fixture expects non-streaming checkpoint steps"
        status, value = MODEL.reply(body)
        encoded = json.dumps(value).encode()
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Cancellation deliberately closes an in-flight model request.


class Harness:
    def __init__(self, binary):
        self.binary = binary.resolve()
        self.run = ROOT / ".local" / ("manager-smoke-" + uuid.uuid4().hex[:8])
        self.run.mkdir(parents=True, mode=0o700)
        self.workspace = self.run / "workspace"
        self.workspace.mkdir()
        self.runtime = tempfile.TemporaryDirectory(prefix="db-mgr-", dir="/tmp")
        self.session = "manager-" + uuid.uuid4().hex[:8]
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith(("HC_", "HERDR_")) or key in {"SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY", "SSH_ORIGINAL_COMMAND"}:
                self.env.pop(key, None)
        home = Path(self.runtime.name) / "home"
        home.mkdir()
        self.env.update(HOME=str(home), SHELL="/bin/sh", XDG_CONFIG_HOME=self.runtime.name, XDG_STATE_HOME=self.runtime.name + "/state")
        self.feishu = ThreadingHTTPServer(("127.0.0.1", 0), fixture.Handler)
        fixture.FIXTURE.server = self.feishu
        self.model = ThreadingHTTPServer(("127.0.0.1", 0), ModelHandler)
        for server in (self.feishu, self.model):
            threading.Thread(target=server.serve_forever, daemon=True).start()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.log = (self.run / "daemon.log").open("w")
        self.daemon = None
        self.config = None
        self.project = None
        self.checks = []

    def boot(self):
        self.daemon = subprocess.Popen([str(self.binary), "--data-dir", str(self.run), "serve", "--port", str(self.port), "--herdr-session", self.session, "--feishu-base", f"http://127.0.0.1:{self.feishu.server_port}"], env=self.env, stdout=self.log, stderr=self.log)
        self.config = fixture.wait_for(lambda: json.loads((self.run / "config.json").read_text()) if (self.run / "config.json").exists() else None, "no manager fixture config")
        fixture.wait_for(lambda: self.api("/state"), "manager fixture daemon not ready")

    def api(self, path, data=None, method=None, token=None):
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}/api" + path, data=None if data is None else json.dumps(data).encode(), headers={"Content-Type": "application/json", "Authorization": "Bearer " + (token or self.config["token"])}, method=method or ("POST" if data is not None else "GET"))
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{path}: HTTP {error.code}: {error.read().decode()}") from error

    def create(self):
        self.project = self.api("/projects", {"name": "Manager protocol fixture", "path": str(self.workspace), "agent": "custom", "extra_args": [sys.executable, str(ROOT / "scripts/mock_agent.py"), "{bridge_exe}", "{credentials}", "{native_session}", "{instructions}"]})
        self.key = self.project["id"]
        self.api(f"/projects/{self.key}/bot", {"app_id": "cli_fixture", "app_secret": "fixture-secret"}, "PUT")
        fixture.wait_for(lambda: fixture.FIXTURE.connections, "no fixture SDK connection")

    def close(self):
        MODEL.release.set()
        if self.daemon and self.daemon.poll() is None:
            self.daemon.terminate()
            self.daemon.wait(timeout=15)
        if self.config:
            subprocess.run([self.config["herdr"], "--session", self.session, "server", "stop"], env=self.env, capture_output=True, timeout=15)
        fixture.FIXTURE.disconnect()
        self.feishu.shutdown()
        self.model.shutdown()
        self.log.close()
        self.runtime.cleanup()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    harness = Harness(args.binary)
    try:
        harness.boot()
        harness.create()
        run_checks(harness)
        result = {"passed": True, "checks": harness.checks, "run_dir": str(harness.run)}
        results = ROOT / "test-results"
        results.mkdir(exist_ok=True, mode=0o700)
        (results / "manager-smoke.json").write_text(json.dumps(result, indent=2))
        print(json.dumps(result, indent=2))
    finally:
        harness.close()


def run_checks(h):
    route = f"/projects/{h.key}"
    initial = h.api(route + "/manager")
    assert initial["mode"] == "native", initial
    provider = {"base_url": f"http://127.0.0.1:{h.model.server_port}/v1", "model": "fixture-manager", "api_key": "manager-fixture-secret", "expected_revision": initial["revision"]}
    configured = h.api(route + "/manager", provider, "PUT")
    assert configured["mode"] == "native" and configured["has_api_key"] is True
    assert "manager-fixture-secret" not in json.dumps(configured)
    assert "manager-fixture-secret" not in json.dumps(h.api("/state"))
    mode = {"mode": "manager", "request_id": "fixture-manager-enable", "expected_revision": configured["revision"]}
    enabled = h.api(route + "/agent/mode", mode)
    assert h.api(route + "/agent/mode", mode) == enabled, "mode request retry changed its result"
    assert h.api(route + "/manager")["mode"] == "manager"
    h.checks.append("opt-in mode, native default, private separate API credential and idempotent mode request")

    MODEL.script(
        {"tool": "write_file", "id": "same-provider-call-id", "arguments": {"path": "manager-note.txt", "content": "restricted manager fixture\n"}},
        {"tool": "read_file", "id": "same-provider-call-id", "arguments": {"path": "manager-note.txt"}},
        {"tool": "send_md", "arguments": {"to": "oc_fixture", "reply_to_message_id": "om_manager_write", "text": "manager-file-roundtrip-ok"}},
        {"tool": "send_file", "arguments": {"to": "oc_fixture", "reply_to_message_id": "om_manager_write", "path": "manager-note.txt"}},
        {"text": ""},
    )
    fixture.FIXTURE.emit(fixture.message("om_manager_write", "Write and read a project note, then reply in this topic."))
    fixture.wait_for(lambda: any("manager-file-roundtrip-ok" in m["content"] for m in fixture.FIXTURE.messages), "manager file/message loop did not finish")
    assert (h.workspace / "manager-note.txt").read_text() == "restricted manager fixture\n"
    sent = [m for m in fixture.FIXTURE.messages if "manager-file-roundtrip-ok" in m["content"]]
    assert len(sent) == 1 and sent[0]["fixture_reply_to"] == "om_manager_write", sent
    fixture.wait_for(lambda: any(m.get("msg_type") == "file" and m.get("fixture_reply_to") == "om_manager_write" for m in fixture.FIXTURE.messages), "manager did not send its project file into the same topic")
    assert fixture.FIXTURE.uploads and b"restricted manager fixture" in fixture.FIXTURE.uploads[-1]
    assert not h.api(route + "/work")["runs"], "raw manager input was also dispatched to native CLI"
    advertised = {t["function"]["name"] for t in MODEL.requests[0]["tools"]}
    assert advertised == {"read_file", "write_file", "delegate", "worker_status", "steer_worker", "send_md", "send_file"}, advertised
    h.checks.append("scripted model → bounded file write/read → real SDK topic outbox; no raw native delivery or Bash tool")

    MODEL.script(
        {"tool": "write_file", "arguments": {"path": "../escape.txt", "content": "must stay in workspace"}},
        {"tool": "send_md", "arguments": {"to": "oc_fixture", "reply_to_message_id": "om_manager_boundary", "text": "boundary-denial-recovered"}},
        {"text": ""},
    )
    fixture.FIXTURE.emit(fixture.message("om_manager_boundary", "Exercise a rejected file effect and continue the conversation."))
    fixture.wait_for(lambda: any("boundary-denial-recovered" in m["content"] for m in fixture.FIXTURE.messages), "denied file effect prevented normal manager recovery")
    assert not (h.run / "escape.txt").exists()
    h.checks.append("workspace escape denied through real model tool protocol without ending subsequent messaging")

    # Pause an in-flight request, then send fresh input before its stale tool call
    # returns. The delayed response must never admit a workspace write.
    MODEL.script({"wait": True, "tool": "write_file", "arguments": {"path": "stale-write.txt", "content": "must not be written"}}, trigger="om_manager_stale")
    fixture.FIXTURE.emit(fixture.message("om_manager_stale", "Start the controlled delayed model request."))
    assert MODEL.started.wait(10), "delayed provider request did not start"
    MODEL.script({"tool": "send_md", "arguments": {"to": "oc_fixture", "reply_to_message_id": "om_manager_fresh", "text": "fresh-steer-ok"}}, {"text": ""}, trigger="om_manager_fresh")
    fixture.FIXTURE.emit(fixture.message("om_manager_fresh", "Supersede the delayed request immediately."))
    fixture.wait_for(lambda: any("fresh-steer-ok" in m["content"] for m in fixture.FIXTURE.messages), "fresh manager steer waited for the stale provider")
    MODEL.release.set()
    fixture.wait_for(lambda: h.api(route + "/manager")["status"] not in {"running", "working", "model", "tools"}, "manager did not settle after steering")
    assert not (h.workspace / "stale-write.txt").exists(), "late model tool wrote after supersession"
    h.checks.append("fresh input cancels blocked model request; superseded response cannot admit a file effect")

    MODEL.script({"tool": "delegate", "arguments": {"instructions": "Perform manager-native-delegation-fixture and reply to the trusted source topic."}}, {"text": ""})
    fixture.FIXTURE.emit(fixture.message("om_manager_delegate", "Delegate this coding task to the native worker."))
    fixture.wait_for(lambda: any(e["source"] == "management" and e["status"] == "delivered" for e in h.api(route + "/work")["events"]), "manager did not wake and delegate to the real HERDR worker", timeout=55)
    fixture.wait_for(lambda: any("manager-native-delegation-fixture" in m["content"] for m in fixture.FIXTURE.messages), "native fixture did not receive the manager directive")
    generation = h.api(route)["project"]["generation"]
    assert h.api(route)["project"]["ready"]
    h.checks.append("manager delegation wakes isolated native coding worker with real HERDR and exactly three MCP tools")

    MODEL.script({"error": True}, trigger="om_manager_unavailable")
    fixture.FIXTURE.emit(fixture.message("om_manager_unavailable", "Exercise a manager provider outage."))
    fixture.wait_for(lambda: h.api(route + "/manager")["status"] in {"failed", "error", "attention", "unavailable"}, "provider outage not surfaced")
    assert h.api(route + "/models"), "deterministic model control unavailable during manager outage"
    assert h.api(route)["project"]["generation"] == generation
    h.checks.append("provider outage surfaces status while native worker identity and deterministic model controls remain available")

    # Restart the daemon with its private database; completed external effects
    # and native continuity must survive without replaying a sent topic message.
    before = len([m for m in fixture.FIXTURE.messages if "manager-file-roundtrip-ok" in m["content"]])
    h.daemon.terminate()
    h.daemon.wait(timeout=15)
    h.boot()
    assert h.api(route + "/manager")["mode"] == "manager"
    assert len([m for m in fixture.FIXTURE.messages if "manager-file-roundtrip-ok" in m["content"]]) == before
    assert "manager-fixture-secret" not in (h.run / "daemon.log").read_text()
    h.checks.append("private daemon restart retains mode and sent effects without credential leakage or message replay")

    current = h.api(route + "/manager")
    native_request = {"mode": "native", "request_id": "fixture-native-enable", "expected_revision": current["revision"]}
    h.api(route + "/agent/mode", native_request)
    assert h.api(route + "/manager")["mode"] == "native"
    fixture.FIXTURE.emit(fixture.message("om_after_manager", "native-mode-after-manager"))
    fixture.wait_for(lambda: any("native-mode-after-manager" in m["content"] for m in fixture.FIXTURE.messages), "native mode failed to resume after manager provider outage", timeout=55)
    assert len([m for m in fixture.FIXTURE.messages if "manager-file-roundtrip-ok" in m["content"] and m.get("fixture_reply_to") == "om_manager_write"]) == before, "mode handoff replayed an already completed manager reply"
    h.checks.append("provider-independent switch to native hands off preserved manager state and resumes new input without replaying its completed effects")


if __name__ == "__main__":
    main()
