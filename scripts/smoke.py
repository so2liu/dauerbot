"""No external Python dependencies. Exercises real SDK WebSocket, HERDR, MCP and HTTP.

Run after frontend/build and cargo build:
  python3 scripts/smoke.py
  python3 scripts/smoke.py --browser
Only its private HC_SESSION, terminals and fixture processes are cleaned up.
"""
import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import threading
import time
import tempfile
import urllib.request
import urllib.error
import uuid

ROOT = Path(__file__).resolve().parent.parent


def varint(value):
    out = b""
    while value > 127:
        out += bytes([(value & 127) | 128]); value >>= 7
    return out + bytes([value])


def scalar(field, value):
    return varint(field << 3) + varint(value)


def blob(field, value):
    if isinstance(value, str):
        value = value.encode()
    return varint((field << 3) | 2) + varint(len(value)) + value


def event_frame(event):
    headers = [("type", "event"), ("message_id", event["event"]["message"]["message_id"]), ("sum", "1"), ("seq", "0")]
    return (scalar(1, 1) + scalar(2, 1) + scalar(3, 1) + scalar(4, 1)
            + b"".join(blob(5, blob(1, k) + blob(2, v)) for k, v in headers)
            + blob(6, "none") + blob(7, "application/json") + blob(8, json.dumps(event)))


def proto_fields(data):
    def integer(pos):
        n = 0; shift = 0
        while True:
            c = data[pos]; pos += 1; n |= (c & 127) << shift
            if c < 128:
                return n, pos
            shift += 7
    pos = 0; fields = {}
    while pos < len(data):
        key, pos = integer(pos)
        if key & 7 == 0:
            value, pos = integer(pos)
        elif key & 7 == 2:
            size, pos = integer(pos); value = data[pos:pos + size]; pos += size
        else:
            raise ValueError("unsupported fixture protobuf wire type")
        fields.setdefault(key >> 3, []).append(value)
    return fields


def ws_frame(data, opcode=2):
    size = len(data)
    return bytes([0x80 | opcode]) + (bytes([size]) if size < 126 else b"\x7e" + struct.pack("!H", size) if size <= 65535 else b"\x7f" + struct.pack("!Q", size)) + data


def exact(conn, size):
    out = b""
    while len(out) < size:
        part = conn.recv(size - len(out))
        if not part:
            raise EOFError()
        out += part
    return out


class Fixture:
    def __init__(self):
        self.connections = []
        self.lock = threading.Lock()
        self.messages = []
        self.uploads = []
        self.acks = 0
        self.connects = 0
        self.server = None

    def emit(self, event):
        with self.lock:
            for conn in self.connections[:]:
                try:
                    conn.sendall(ws_frame(b"heartbeat", 9))
                    conn.sendall(ws_frame(event_frame(event)))
                except OSError:
                    pass

    def disconnect(self):
        with self.lock:
            for conn in self.connections[:]:
                try:
                    conn.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass


FIXTURE = Fixture()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers(); self.wfile.write(data)

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path.endswith("/callback/ws/endpoint"):
            port = FIXTURE.server.server_port
            self.reply({"code": 0, "msg": "ok", "data": {"URL": f"ws://127.0.0.1:{port}/ws?service_id=1", "ClientConfig": {"PingInterval": 15}}})
        elif self.path.endswith("/tenant_access_token/internal"):
            self.reply({"code": 0, "tenant_access_token": "fixture-token", "expire": 7200})
        elif self.path.endswith("/files"):
            assert self.headers.get("Authorization") == "Bearer fixture-token"
            FIXTURE.uploads.append(data)
            self.reply({"code": 0, "data": {"file_key": "fixture-file-key"}})
        elif self.path.startswith("/open-apis/im/v1/messages"):
            assert self.headers.get("Authorization") == "Bearer fixture-token"
            value = json.loads(data)
            assert value["receive_id"] in {"oc_fixture", "oc_private", "ou_fixture", "oc_new_group"}, value
            FIXTURE.messages.append(value)
            self.reply({"code": 0, "data": {"message_id": "om_sent_" + str(len(FIXTURE.messages))}})
        else:
            self.reply({"code": 404, "msg": "unimplemented fixture endpoint"})

    def do_GET(self):
        if self.path.startswith("/open-apis/im/v1/chats/oc_fixture/members"):
            self.reply({"code":0,"data":{"items":[{"member_id":"ou_fixture","name":"测试用户"}],"has_more":False}}); return
        if self.path == "/open-apis/im/v1/chats/oc_fixture":
            self.reply({"code":0,"data":{"name":"端到端测试群"}}); return
        if "/resources/" in self.path:
            data = b"downloaded fixture attachment"
            self.send_response(200); self.send_header("Content-Length", str(len(data)))
            self.send_header("Content-Type", "application/octet-stream")
            self.end_headers(); self.wfile.write(data); return
        if not self.path.startswith("/ws"):
            self.reply({"code": 404}); return
        accept = base64.b64encode(hashlib.sha1((self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket"); self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept); self.end_headers()
        conn = self.connection
        with FIXTURE.lock:
            FIXTURE.connections.append(conn); FIXTURE.connects += 1
        try:
            while True:
                a, b = exact(conn, 2)
                size = b & 127
                if size == 126:
                    size = struct.unpack("!H", exact(conn, 2))[0]
                elif size == 127:
                    size = struct.unpack("!Q", exact(conn, 8))[0]
                mask = exact(conn, 4) if b & 128 else None
                data = exact(conn, size)
                if mask:
                    data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
                if a & 15 == 8:
                    break
                if a & 15 == 2:
                    fields = proto_fields(data)
                    if 8 in fields:
                        try:
                            if json.loads(fields[8][0]).get("code") == 200:
                                FIXTURE.acks += 1
                        except (ValueError, UnicodeDecodeError):
                            pass
        except (EOFError, OSError):
            pass
        finally:
            with FIXTURE.lock:
                if conn in FIXTURE.connections:
                    FIXTURE.connections.remove(conn)
            self.close_connection = True


def wait_for(predicate, message, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(.15)
    raise AssertionError(message)


def message(source, text, kind="text", content=None, chat_type="group", chat_id="oc_fixture"):
    return {"schema": "2.0", "header": {"event_type": "im.message.receive_v1", "event_id": str(uuid.uuid4())},
            "event": {"sender": {"sender_type": "user", "sender_id": {"open_id": "ou_fixture"}},
                      "message": {"chat_id": chat_id, "chat_type": chat_type, "message_id": source, "message_type": kind,
                                  "content": json.dumps(content if content is not None else {"text": text})}}}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--browser", action="store_true")
    parser.add_argument("--binary", type=Path, help="use an already built bridge executable")
    parser.add_argument("--real-pi", action="store_true", help="one real model turn using the locally configured Pi account")
    parser.add_argument("--real-agent",choices=["codex","claude","pi"],help="one real CLI/model turn; may incur model charges")
    args = parser.parse_args()
    real_agent=args.real_agent or ("pi" if args.real_pi else None)
    binary = args.binary or ROOT / "target/debug/hfb"
    assert binary.exists(), "run cargo build first"
    run = ROOT / ".local" / ("smoke-" + uuid.uuid4().hex[:8])
    run.mkdir(parents=True, mode=0o700)
    workspace = run / "workspace"; workspace.mkdir()
    (workspace / "report.txt").write_text("Fixture report: streamed from disk.\n")
    results = ROOT / "test-results"; results.mkdir(exist_ok=True,mode=0o700); results.chmod(0o700)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("HC_"):
            env.pop(key)
    env.update(XDG_CONFIG_HOME=str(run / "xdg-config"), XDG_STATE_HOME=str(run / "xdg-state"))
    # macOS Unix sockets have a short path limit; keep the isolated endpoint short.
    runtime_dir = tempfile.TemporaryDirectory(prefix="hfb-smoke-")
    env["XDG_RUNTIME_DIR"] = runtime_dir.name
    session = "hfb-smoke-" + uuid.uuid4().hex[:8]
    env["HC_SESSION"] = session
    FIXTURE.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=FIXTURE.server.serve_forever, daemon=True).start()
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); port = sock.getsockname()[1]; sock.close()
    log = (run / "daemon.log").open("w")
    daemon = None
    checks = []
    try:
        def boot():
            return subprocess.Popen([str(binary), "--data-dir", str(run), "serve", "--port", str(port), "--core-session", session,
                                     "--feishu-base", f"http://127.0.0.1:{FIXTURE.server.server_port}"], env=env, stdout=log, stderr=log)
        daemon = boot()
        config = wait_for(lambda: json.loads((run / "config.json").read_text()) if (run / "config.json").exists() else None, "no daemon config")
        base = f"http://127.0.0.1:{port}"

        def api(path, data=None, method=None, auth=True):
            headers = {"Content-Type": "application/json"}
            if auth:
                headers["Authorization"] = "Bearer " + config["token"]
            req = urllib.request.Request(base + "/api" + path, data=None if data is None else json.dumps(data).encode(),
                                         headers=headers, method=method or ("POST" if data is not None else "GET"))
            try:
                with urllib.request.urlopen(req, timeout=100) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                raise RuntimeError(f"{path}: {error.read().decode()}") from error

        wait_for(lambda: api("/state"), "daemon not ready")
        api("/settings", {"app_id": "cli_fixture", "app_secret": "fixture-secret"}, "PUT")
        wait_for(lambda: FIXTURE.connections, "SDK did not establish WebSocket")
        checks.append("real SDK WebSocket endpoint discovery and handshake")
        if real_agent=="pi":
            extra_args=["--approve"]
        elif real_agent=="codex":
            extra_args=[]
        elif real_agent:
            extra_args=[]
        else:
            extra_args=[sys.executable,str(ROOT/"scripts/mock_agent.py"),"{bridge_exe}","{credentials}"]
        project = api("/projects", {"name": "端到端验证项目", "path": str(workspace), "agent": real_agent or "custom",
                                   "extra_args": extra_args})
        key = project["id"]
        api(f"/projects/{key}/start", {}, "POST")
        wait_for(lambda: api(f"/projects/{key}")["project"]["ready"], "real MCP did not register readiness",timeout=100 if real_agent else 35)
        checks.append("isolated real HERDR terminal + real MCP initialize/tools/list/session_ready")
        FIXTURE.emit(message("om_first", "请先确认本消息，调用 send_markdown 发送一条带 **测试** 的消息，再调用 send_file 发送项目目录的 report.txt（send-file）。这是开发验证，只需完成这两项。"))
        wait_for(lambda: len(FIXTURE.messages) >= 2, "agent did not send Markdown and file",timeout=120 if real_agent else 35)
        assert FIXTURE.uploads and b"Fixture report" in FIXTURE.uploads[0]
        records = api(f"/projects/{key}")["messages"]
        assert next(m for m in records if m["source_id"] == "om_first")["status"] == "acknowledged"
        checks.append("WebSocket → durable inbox → PTY → MCP ack → Markdown/file HTTP delivery")
        if real_agent:
            (results / f"real-{real_agent}.json").write_text(json.dumps({"passed": True,"checks": checks,"project":project["id"],"run_dir":str(run)},ensure_ascii=False,indent=2))
            print(f"Real {real_agent} model + MCP + HERDR + simulated Feishu round trip passed.")
            return
        before = len(FIXTURE.messages)
        FIXTURE.emit(message("om_first", "duplicate payload"))
        wait_for(lambda: FIXTURE.acks >= 2, "duplicate event did not receive SDK ack")
        assert len([m for m in api(f"/projects/{key}")["messages"] if m["source_id"] == "om_first"]) == 1
        assert len(FIXTURE.messages) == before
        checks.append("duplicate Feishu message is acknowledged without duplicate execution")
        FIXTURE.emit(message("om_second", "second message"))
        FIXTURE.emit(message("om_third", "", kind="file", content={"file_key": "fixture-inbound", "file_name": "incoming.txt"}))
        wait_for(lambda: all(any(m["source_id"] == key and m["status"] == "acknowledged" for m in api(f"/projects/{project['id']}")["messages"]) for key in ["om_second", "om_third"]), "queued messages failed")
        assert list((run / "files").glob("*incoming.txt"))
        checks.append("ordered follow-up turns and streamed inbound attachment")
        FIXTURE.emit(message("om_private", "direct-to-user", chat_type="p2p", chat_id="oc_private"))
        wait_for(lambda: any(c["chat_id"]=="oc_private" for c in api("/conversations")["items"]), "new private chat was not discovered")
        assert api("/state")["bot_project"] == key
        wait_for(lambda: any(m["receive_id"]=="ou_fixture" for m in FIXTURE.messages), "agent-selected user destination failed")
        wait_for(lambda: any(m["source_id"]=="om_private" and m["status"]=="acknowledged" for m in api(f"/projects/{key}")["messages"]), "private message did not join the same main session")
        checks.append("new private chat enters fixed bot project without chat binding + XML context + MCP explicit user to")
        FIXTURE.emit(message("om_new_group", "new group without binding", chat_id="oc_new_group"))
        wait_for(lambda: any(m["source_id"]=="om_new_group" and m["status"]=="acknowledged" for m in api(f"/projects/{key}")["messages"]), "new group did not automatically enter bot project")
        checks.append("new group automatically enters the same bot project without chat configuration")
        count = FIXTURE.connects; FIXTURE.disconnect()
        wait_for(lambda: FIXTURE.connects > count, "daemon did not reconnect WebSocket")
        checks.append("daemon supervises SDK and reconnects after forced disconnection")
        api(f"/projects/{key}/pause", {"paused": True})
        FIXTURE.emit(message("om_after_restart", "persist across daemon restart"))
        wait_for(lambda: any(m["source_id"] == "om_after_restart" for m in api(f"/projects/{key}")["messages"]), "paused event did not persist")
        old_terminal = api(f"/projects/{key}")["project"]["terminal"]
        daemon.terminate(); daemon.wait(timeout=15); daemon = boot()
        wait_for(lambda: api("/state"), "daemon did not restart")
        assert api(f"/projects/{key}")["project"]["terminal"] == old_terminal
        api(f"/projects/{key}/pause", {"paused": False})
        wait_for(lambda: any(m["source_id"] == "om_after_restart" and m["status"] == "acknowledged" for m in api(f"/projects/{key}")["messages"]), "persistent queue did not resume")
        checks.append("daemon restart preserves agent terminal and resumes persistent paused queue")
        subprocess.run(["hc", "server", "restart"], env=env, check=True, capture_output=True, timeout=20)
        FIXTURE.emit(message("om_core_restart", "core restart"))
        wait_for(lambda: any(m["source_id"] == "om_core_restart" and m["status"] == "acknowledged" for m in api(f"/projects/{key}")["messages"]), "core restart lost conversation")
        checks.append("isolated Core restart preserves original agent and message flow")
        cli = subprocess.run([str(binary), "--data-dir", str(run), "project", "list"], capture_output=True, text=True, timeout=10)
        assert cli.returncode == 0 and "端到端验证项目" in cli.stdout
        checks.append("CLI and UI API share the same project state")
        if args.browser:
            browser = results / "e2e.json"
            browser.write_text(json.dumps({"url": base, "token": config["token"], "workspace": str(workspace), "project_id": key}))
            browser.chmod(0o600)
            subprocess.run(["pnpm", "test:e2e"], cwd=ROOT / "frontend", check=True, timeout=120)
            checks.append("desktop/mobile browser CRUD, queue controls and screenshots")
        # Stopped sessions must retain later messages rather than execute them in a shell.
        api(f"/projects/{key}/stop", {}, "POST")
        FIXTURE.emit(message("om_stopped", "do not deliver to a stopped terminal"))
        wait_for(lambda: any(m["source_id"] == "om_stopped" and m["status"] == "queued" for m in api(f"/projects/{key}")["messages"]), "stopped-session message was not queued")
        checks.append("stopped agent does not consume later messages")
        (results / "smoke.json").write_text(json.dumps({"passed": True, "checks": checks, "run_dir": str(run)}, ensure_ascii=False, indent=2))
        print(json.dumps({"passed": True, "checks": checks, "run_dir": str(run)}, ensure_ascii=False, indent=2))
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate(); daemon.wait(timeout=15)
        subprocess.run(["hc", "server", "stop"], env=env, capture_output=True, timeout=15)
        FIXTURE.disconnect(); FIXTURE.server.shutdown(); log.close()
        runtime_dir.cleanup()


if __name__ == "__main__":
    main()
