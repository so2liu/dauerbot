"""Actual Pi parent/child CLIs against a deterministic localhost model fixture.

Run: python3 scripts/pi-native-subagent-fixture.py /absolute/path/to/pi [pinned-herdr-hook]
No external model, credentials, live configuration or HERDR pane is used.
"""
import json
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

child_started = threading.Event()
release_child = threading.Event()
root_replied = threading.Event()
fence_replied = threading.Event()
requests = []


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(request)
        messages = request["messages"]
        is_child = any("Task: fixture-child" in str(m.get("content", "")) for m in messages if m["role"] == "user")
        has_result = any(m["role"] == "tool" for m in messages)
        fence_requested = any("fixture-fence" in str(m.get("content", "")) for m in messages if m["role"] == "user")
        fence_refused = any("This managed session is retiring" in str(m.get("content", "")) for m in messages if m["role"] == "tool")
        if is_child:
            child_started.set()
            if not release_child.wait(15):
                self.send_error(500, "Fixture child release timed out")
                return
            delta, reason = {"role": "assistant", "content": "actual child Pi completed"}, "stop"
        elif fence_requested and not fence_refused:
            delta, reason = {"role": "assistant", "tool_calls": [{"index": 0, "id": "fixture-fenced-call", "type": "function", "function": {"name": "dauerbot_subagent", "arguments": json.dumps({"agent": "worker", "task": "fixture-blocked"})}}]}, "tool_calls"
        elif not has_result:
            assert any(t["function"]["name"] == "dauerbot_subagent" for t in request["tools"])
            delta, reason = {"role": "assistant", "tool_calls": [{"index": 0, "id": "fixture-child-call", "type": "function", "function": {"name": "dauerbot_subagent", "arguments": json.dumps({"agent": "worker", "task": "fixture-child"})}}]}, "tool_calls"
        else:
            delta, reason = {"role": "assistant", "content": "actual parent Pi replied"}, "stop"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for body in [
            {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}},
        ]:
            self.wfile.write(("data: " + json.dumps({"id": "fixture", "object": "chat.completion.chunk", "created": 1, "model": "fixture-model", **body}) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        if not is_child and has_result and reason == "stop":
            root_replied.set()
            if fence_refused:
                fence_replied.set()


with tempfile.TemporaryDirectory(prefix="db-pi-model-") as tmp:
    root = Path(tmp)
    managed = root / "managed"
    shutil.copytree(Path(__file__).resolve().parent.parent / "assets/pi-subagent", managed)
    generation = str(uuid.uuid4())
    (managed / "managed.json").write_text(json.dumps({"generation": generation, "incomplete": False}))
    agent = root / "agent"
    agent.mkdir()
    # Reuse existing tool binaries; a private HOME must not trigger downloads.
    (agent / "bin").mkdir()
    for tool in ("fd", "rg"):
        cached = shutil.which(tool) or str(Path.home() / ".pi/agent/bin" / tool)
        assert Path(cached).is_file(), f"Preinstall {tool}; this fixture never downloads tools"
        shutil.copy2(cached, agent / "bin" / tool)
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    print(json.dumps({"fixtureServer": {"pid": os.getpid(), "thread": "ThreadingHTTPServer", "host": "127.0.0.1", "port": model.server_port}, "cleanup": "owned by finite fixture finally block"}), flush=True)
    (agent / "settings.json").write_text(json.dumps({"defaultProvider": "fixture", "defaultModel": "fixture-model"}))
    (agent / "models.json").write_text(json.dumps({"providers": {"fixture": {"baseUrl": f"http://127.0.0.1:{model.server_port}/v1", "api": "openai-completions", "apiKey": "fixture-only", "models": [{"id": "fixture-model"}]}}}))
    if len(sys.argv) > 2:
        (agent / "extensions").mkdir()
        shutil.copyfile(sys.argv[2], agent / "extensions/herdr-agent-state.ts")
    home = root / "home"
    home.mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "SHELL": "/bin/sh",
           "XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"),
           "PI_CODING_AGENT_DIR": str(agent), "TERM": "xterm-256color", "NO_COLOR": "1", "LANG": "en_US.UTF-8"}
    master, slave = pty.openpty()
    proc = subprocess.Popen([sys.argv[1], "--offline", "--approve", "--session-id", generation, "--extension", str(managed / "managed.ts"), "fixture-root"], cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    os.close(slave)
    output = bytearray()

    def wait_until(predicate, message, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([master], [], [], .05)[0]:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    # PTY EOF can precede waitpid visibility by one scheduler turn.
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
                    if predicate():
                        return
                    break
            if predicate():
                return
            if proc.poll() is not None:
                break
        raise AssertionError(f"{message}\n{output.decode(errors='replace')[-5000:]}")

    def evidence():
        return json.loads((managed / "children.json").read_text())

    try:
        wait_until(child_started.is_set, "Actual child Pi did not reach the local model fixture")
        active = evidence()
        assert active["generation"] == generation and active["session"] == generation
        assert active["state"] == "active", active
        assert not root_replied.is_set(), "Parent must await its active child tool"
        os.write(master, b"fixture-steering\r")
        assert not root_replied.is_set(), "Input delivery is not an immediate root model reply"
        release_child.set()
        wait_until(root_replied.is_set, "Actual parent Pi did not receive its child result")
        wait_until(lambda: evidence()["state"] == "quiescent", "Actual child Pi completion did not release coverage")
        assert evidence()["revision"] >= active["revision"] + 1
        assert len(requests) >= 3
        assert all(r["model"] == "fixture-model" for r in requests), "Parent model must be inherited by actual child Pi"
        assert evidence()["session"] == generation, "Headless child must not replace the native parent's identity"
        # The native adapter must refuse a later child dispatch after the daemon's
        # generation fence, including when the root receives fresh input.
        (managed / "retiring").write_text("fixture native retirement claim")
        os.write(master, b"fixture-fence\r")
        wait_until(fence_replied.is_set, "Native parent did not receive the managed dispatch veto")
        assert sum("Task: fixture-child" in str(r["messages"]) for r in requests) == 1
        assert not any("Task: fixture-blocked" in str(r["messages"]) for r in requests), "A fenced native child reached the model"
        assert evidence()["state"] == "quiescent"
        os.write(master, b"/quit\r")
        wait_until(lambda: proc.poll() is not None, "Actual parent Pi did not shut down", timeout=8)
        assert not (managed / "children.json").exists()
        print(json.dumps({"actualPi": subprocess.check_output([sys.argv[1], "--version"], text=True).strip(), "checks": "real TUI parent and headless child Pi; localhost deterministic model; active/quiescent evidence; model inheritance; foreground response barrier; retirement gate veto; parent identity; native shutdown", "liveModel": False}))
    finally:
        release_child.set()
        try:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait(timeout=5)
        finally:
            os.close(master)
            model.shutdown()
            model.server_close()
            print(json.dumps({"fixtureServerClosed": model.server_port, "parentExited": proc.poll() is not None}), flush=True)
