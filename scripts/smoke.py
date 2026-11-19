"""No external Python dependencies. Exercises real SDK WebSocket, HERDR, MCP and HTTP.

Run after cargo build:
  python3 scripts/smoke.py
Only its private legacy HERDR session, terminals and fixture processes are cleaned up.
"""
import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import selectors
import datetime
import struct
import subprocess
import sys
import threading
import time
import tempfile
import urllib.request
import urllib.error
import urllib.parse
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


def event_frame(event, frame_id=None):
    message = event.get("event", {}).get("message", {})
    identity = frame_id or message.get("message_id") or event["header"]["event_id"]
    headers = [("type", "event"), ("message_id", identity), ("sum", "1"), ("seq", "0")]
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
        self.connection_bots = {}
        self.lock = threading.Lock()
        self.messages = []
        self.message_destinations = {}
        self.uploads = []
        self.acks = 0
        self.connects = 0
        self.server = None
        self.issued_tokens = set()
        self.callback_responses = []
        self.resource_started = threading.Event()
        self.resource_release = threading.Event()

    def emit(self, event, app_id="cli_fixture", frame_id=None):
        with self.lock:
            message = event.get("event", {}).get("message")
            if message is not None:
                self.message_destinations[(app_id, message["message_id"])] = message["chat_id"]
            for conn in self.connections[:]:
                if self.connection_bots.get(conn) != app_id:
                    continue
                try:
                    conn.sendall(ws_frame(b"heartbeat", 9))
                    conn.sendall(ws_frame(event_frame(event, frame_id)))
                except OSError:
                    pass

    def emit_callback(self, event, app_id="cli_fixture", frame_id=None):
        assert event["header"]["event_type"] == "card.action.trigger"
        self.emit(event, app_id, frame_id)

    def disconnect(self, app_id=None):
        with self.lock:
            for conn in self.connections[:]:
                if app_id is not None and self.connection_bots.get(conn) != app_id:
                    continue
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
            app_id = json.loads(data)["AppID"]
            self.reply({"code": 0, "msg": "ok", "data": {"URL": f"ws://127.0.0.1:{port}/ws?service_id=1&app_id={app_id}", "ClientConfig": {"PingInterval": 15}}})
        elif self.path.endswith("/tenant_access_token/internal"):
            token = "fixture-token-" + json.loads(data)["app_id"]
            FIXTURE.issued_tokens.add("Bearer " + token)
            self.reply({"code": 0, "tenant_access_token": token, "expire": 7200})
        elif self.path.endswith("/files"):
            assert self.headers.get("Authorization") in FIXTURE.issued_tokens
            FIXTURE.uploads.append(data)
            self.reply({"code": 0, "data": {"file_key": "fixture-file-key"}})
        elif self.path.startswith("/open-apis/im/v1/messages"):
            assert self.headers.get("Authorization") in FIXTURE.issued_tokens
            value = json.loads(data)
            value["fixture_app_id"] = self.headers["Authorization"].removeprefix("Bearer fixture-token-")
            anchor = None
            if self.path.endswith("/reply"):
                anchor = self.path.split("/")[-2]
                assert value["reply_in_thread"] is True
                value["fixture_reply_to"] = anchor
                value["receive_id"] = FIXTURE.message_destinations[(value["fixture_app_id"], anchor)]
            assert value["receive_id"] in {"oc_fixture", "oc_private", "ou_fixture", "oc_new_group"}, value
            FIXTURE.messages.append(value)
            sent = "om_sent_" + str(len(FIXTURE.messages))
            FIXTURE.message_destinations[(value["fixture_app_id"], sent)] = value["receive_id"]
            self.reply({"code": 0, "data": {"message_id": sent,
                "chat_id": "oc_private" if value["receive_id"] == "ou_fixture" else value["receive_id"],
                **({"thread_id": "omt_" + anchor, "root_id": anchor, "parent_id": anchor} if anchor else {})}})
        else:
            self.reply({"code": 404, "msg": "unimplemented fixture endpoint"})

    def do_GET(self):
        if self.path == "/open-apis/bot/v3/info":
            assert self.headers.get("Authorization") in FIXTURE.issued_tokens
            self.reply({"code":0,"bot":{"open_id":"ou_fixture_bot"}}); return
        if self.path.startswith("/open-apis/im/v1/chats/oc_fixture/members"):
            app_id = self.headers["Authorization"].removeprefix("Bearer fixture-token-")
            self.reply({"code":0,"data":{"items":[{"member_id":"ou_fixture","name":"测试用户-" + app_id}],"has_more":False}}); return
        if self.path == "/open-apis/im/v1/chats/oc_fixture":
            self.reply({"code":0,"data":{"name":"端到端测试群"}}); return
        if "/resources/" in self.path:
            if "img_fixture_slow" in self.path:
                FIXTURE.resource_started.set()
                assert FIXTURE.resource_release.wait(20), "slow resource fixture was not released"
            data = (ROOT / "tests/fixtures/input.png").read_bytes() if "type=image" in self.path else b"downloaded fixture attachment"
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
            FIXTURE.connection_bots[conn] = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)["app_id"][0]
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
                            ack = json.loads(fields[8][0])
                            if ack.get("code") == 200:
                                FIXTURE.acks += 1
                            if ack.get("data"):
                                headers = {}
                                for raw in fields.get(5, []):
                                    header = proto_fields(raw)
                                    headers[header[1][0].decode()] = header[2][0].decode()
                                response = json.loads(base64.b64decode(ack["data"]))
                                with FIXTURE.lock:
                                    FIXTURE.callback_responses.append({"frame_id": headers.get("message_id"), "code": ack["code"], "response": response})
                        except (ValueError, UnicodeDecodeError):
                            pass
        except (EOFError, OSError):
            pass
        finally:
            with FIXTURE.lock:
                if conn in FIXTURE.connections:
                    FIXTURE.connections.remove(conn)
                FIXTURE.connection_bots.pop(conn, None)
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


def message(source, text, kind="text", content=None, chat_type="group", chat_id="oc_fixture", addressed=True):
    return {"schema": "2.0", "header": {"event_type": "im.message.receive_v1", "event_id": str(uuid.uuid4())},
            "event": {"sender": {"sender_type": "user", "sender_id": {"open_id": "ou_fixture"}},
                      "message": {"chat_id": chat_id, "chat_type": chat_type, "message_id": source, "message_type": kind,
                                  "mentions": [{"id":{"open_id":"ou_fixture_bot"},"key":"@_user_1","mentioned_type":"bot","name":"Fixture bot"}] if addressed and chat_type=="group" else [],
                                  "content": json.dumps(content if content is not None else {"text": text})}}}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, help="use an already built bridge executable")
    parser.add_argument("--bundled-runtime", action="store_true", help="hide user HERDR and verify offline app-owned provisioning")
    parser.add_argument("--real-pi", action="store_true", help="one real model turn using the locally configured Pi account")
    parser.add_argument("--real-agent",choices=["codex","claude","pi","agy"],help="one real CLI/model turn; may incur model charges")
    parser.add_argument("--real-steering", action="store_true", help="verify busy-turn steering with --real-agent; may incur model charges")
    args = parser.parse_args()
    real_agent=args.real_agent or ("pi" if args.real_pi else None)
    assert not args.real_steering or real_agent, "--real-steering requires a native Agent"
    binary = (args.binary or ROOT / "target/debug/hfb").resolve()
    assert binary.exists(), "run cargo build first"
    run = ROOT / ".local" / ("smoke-" + uuid.uuid4().hex[:8])
    run.mkdir(parents=True, mode=0o700)
    workspace = run / "workspace"; workspace.mkdir()
    (workspace / "report.txt").write_text("Fixture report: streamed from disk.\n")
    results = ROOT / "test-results"; results.mkdir(exist_ok=True,mode=0o700); results.chmod(0o700)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("HC_") or key in ("SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY", "SSH_ORIGINAL_COMMAND"):
            env.pop(key)
    # Keep Unix socket paths below macOS limits and separate from every user service.
    runtime_dir = tempfile.TemporaryDirectory(prefix="db-smoke-", dir="/tmp")
    env.update(XDG_CONFIG_HOME=runtime_dir.name, XDG_STATE_HOME=runtime_dir.name + "/state")
    for name in ("HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "HERDR_SESSION", "HERDR_CONFIG_PATH"):
        env.pop(name, None)
    session = "smoke-" + uuid.uuid4().hex[:8]
    if not real_agent:
        fixture_home = Path(runtime_dir.name) / "home"
        fixture_home.mkdir()
        env.update(HOME=str(fixture_home), SHELL="/bin/sh")
    if args.bundled_runtime:
        assert not real_agent, "bundled-runtime uses private fixture Agents only"
        env["PATH"] = "/usr/bin:/bin:/usr/sbin:/sbin"
        assert not any((Path(p) / "herdr").exists() for p in env["PATH"].split(":"))
    FIXTURE.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=FIXTURE.server.serve_forever, daemon=True).start()
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); port = sock.getsockname()[1]; sock.close()
    log = (run / "daemon.log").open("w")
    daemon = None
    checks = []
    agy_config = Path.home() / ".gemini/config/mcp_config.json"
    agy_original = agy_config.read_bytes() if real_agent == "agy" and agy_config.exists() else None
    if agy_original is not None:
        backup = run / "original-agy-mcp.json"
        backup.write_bytes(agy_original)
        backup.chmod(0o600)
    try:
        if args.bundled_runtime:
            def initialize(data, fixture_env=env):
                subprocess.run([str(binary), "--data-dir", str(data), "init"], env=fixture_env,
                               check=True, capture_output=True, text=True, timeout=20)
                return json.loads((data / "config.json").read_text())
            initial = initialize(run)
            installed = Path(initial["herdr"])
            original = installed.read_bytes()
            installed.write_bytes(b"interrupted copy")
            assert initialize(run)["herdr"] == str(installed)
            assert installed.read_bytes() == original
            installed.chmod(0o600)
            initialize(run)
            assert os.access(installed, os.X_OK), "matching bytes without execute mode were not repaired"
            before = installed.stat().st_mtime_ns
            initialize(run)
            assert installed.stat().st_mtime_ns == before, "repeated init rewrote the runtime"
            reuse_env = dict(env, PATH=str(installed.parent) + ":" + env["PATH"])
            assert initialize(run / "reuse", reuse_env)["herdr"] == str(installed)
            explicit_dir = run / "explicit"
            explicit_dir.mkdir()
            custom = explicit_dir / "user-herdr"
            shutil.copyfile(installed, custom)
            custom.chmod(0o700)
            (explicit_dir / "config.json").write_text(json.dumps({"herdr": str(custom.resolve())}))
            assert initialize(explicit_dir)["herdr"] == str(custom.resolve())
            assert custom.read_bytes() == original
            checks.append("persisted corrupt and non-executable copies repaired; repeated init stable; discovered compatible and explicit installs preserved")
        def boot():
            return subprocess.Popen([str(binary), "--data-dir", str(run), "serve", "--port", str(port), "--herdr-session", session,
                                     "--feishu-base", f"http://127.0.0.1:{FIXTURE.server.server_port}"], env=env, stdout=log, stderr=log)
        daemon = boot()
        config = wait_for(lambda: json.loads((run / "config.json").read_text()) if (run / "config.json").exists() else None, "no daemon config")
        if args.bundled_runtime:
            installed = Path(config["herdr"])
            assert installed.is_relative_to(run.resolve() / "runtime/herdr/0.9.5"), installed
            assert installed.is_file()
            checks.append("offline automatic HERDR install with private HOME/XDG and HERDR hidden from PATH")
        base = f"http://127.0.0.1:{port}"

        def legacy_rpc(method, params=None):
            target = Path(runtime_dir.name) / "herdr" / "sessions" / session / "herdr.sock"
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(5)
                stream.connect(str(target))
                request_id = uuid.uuid4().hex
                stream.sendall((json.dumps({"id": request_id, "method": method, "params": params or {}}) + "\n").encode())
                with stream.makefile("r") as reader:
                    response = json.loads(reader.readline())
                assert "error" not in response, response
                return response["result"]

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
        if not real_agent:
            managed = api("/projects", {"name": "../../私有 工作区", "agent": "codex"})
            managed_path = Path(managed["path"])
            assert managed_path.parent == fixture_home / ".dauerbot/agents", managed_path
            assert managed_path.name.startswith("私有-工作区-")
            assert len(managed_path.name.rsplit("-", 1)[1]) == 4
            assert managed_path.stat().st_mode & 0o777 == 0o700
            assert "HANDOFF.md" in (managed_path / "AGENTS.md").read_text()
            assert (managed_path / ".dauerbot/skills/herdr-cli-subagents/SKILL.md").is_file()
            api(f"/projects/{managed['id']}", method="DELETE")
            checks.append("default project creation provisions a private owned workspace and concise AGENTS.md")
        (workspace / "AGENTS.md").write_text("Fixture user instructions: preserve these rules.\n")

        if real_agent=="pi":
            extra_args=["--approve"]
        elif real_agent=="codex":
            extra_args=[]
        elif real_agent:
            extra_args=[]
        else:
            extra_args=[sys.executable,str(ROOT/"scripts/mock_agent.py"),"{bridge_exe}","{credentials}","{native_session}","{instructions}"]
        project = api("/projects", {"name": "端到端验证项目", "path": str(workspace), "agent": real_agent or "custom",
                                   "extra_args": extra_args})
        key = project["id"]
        assert (workspace / "AGENTS.md").read_text() == "Fixture user instructions: preserve these rules.\n"

        assert api("/state")["projects"][0]["bot"] is None, "a new project inherited a bot"
        api(f"/projects/{key}/bot", {"app_id":"cli_fixture","app_secret":"fixture-secret"}, "PUT")
        wait_for(lambda: FIXTURE.connections, "SDK did not establish WebSocket")
        checks.append("real SDK WebSocket endpoint discovery and handshake")
        api(f"/projects/{key}/start", {}, "POST")
        wait_for(lambda: api(f"/projects/{key}")["project"]["ready"], "real MCP did not register readiness",timeout=100 if real_agent else 35)
        instructions = (workspace / "AGENTS.md").read_text()
        assert instructions.startswith("Fixture user instructions: preserve these rules.\n")
        assert instructions.count("<!-- dauerbot:delegation:begin -->") == 1
        local_skill = workspace / ".dauerbot/skills/herdr-cli-subagents/SKILL.md"
        assert "External Herdr panes are not covered" in local_skill.read_text()
        checks.append("existing project startup preserves user rules and installs one project-local delegation block and skill")
        checks.append("isolated real HERDR terminal + real MCP initialize/tools/list with exactly three tools")
        if args.bundled_runtime:
            actual = wait_for(lambda: (workspace / ".fixture-herdr").read_text() if (workspace / ".fixture-herdr").exists() else None, "no Agent runtime PATH evidence")
            assert actual == config["herdr"], (actual, config["herdr"])
            descriptor = api(f"/projects/{key}/terminal")
            assert descriptor["herdr"] == config["herdr"], descriptor
            checks.append("Agent subprocess PATH and native terminal descriptor use the resolved private HERDR")
        if not real_agent:
            # Custom fixtures have no supported CLI-native session hook. Seed
            # only their verified persisted marker, never a real user's store.
            # Native CLI acceptance obtains this metadata from HERDR instead.
            def fixture_continuity():
                marker = workspace / ".fixture-session"
                if not marker.exists():
                    return False
                launches_path = workspace / ".fixture-launches"
                if not launches_path.exists():
                    return False
                launch = json.loads(launches_path.read_text().splitlines()[-1])
                native = marker.read_text()
                if launch["native"] != native:
                    return False
                with sqlite3.connect(run / "bridge.sqlite3") as conn:
                    current = conn.execute("SELECT generation,terminal FROM projects WHERE id=?", (key,)).fetchone()
                    if not current or current[0] != launch["generation"] or current[1] is None:
                        return False
                    conn.execute("UPDATE projects SET native_session=?,execution=json_set(execution,'$.native_confirmed',json('true')) WHERE id=? AND generation=?",
                                 (native, key, launch["generation"]))
                    conn.execute("UPDATE work_executions SET native_session=? WHERE project_id=? AND generation=?", (native,key,launch["generation"]))
                return True
            wait_for(fixture_continuity, "fixture did not persist its resume marker")

        if args.real_steering:
            FIXTURE.emit(message("om_native_hold",
                "Local steering acceptance check. Do not send Feishu messages or edit files. "
                "Run one foreground shell command: sleep 20. "
                "A second live user message will arrive to change direction. "
                "After the sleep, obey that message instead of inventing further work."))
            def native_hold():
                work = api(f"/projects/{key}/work")
                event = next((e for e in work["events"] if e["source_id"] == "om_native_hold"), None)
                pane = legacy_rpc("pane.get", {"pane_id": api(f"/projects/{key}")["project"]["execution"]["pane_id"]})["pane"]
                return event if event and event["status"] == "submitted" and pane["agent_status"] == "working" else None
            original = wait_for(native_hold, "native Agent did not start the held turn", timeout=120)
            generation = api(f"/projects/{key}")["project"]["generation"]
            FIXTURE.emit(message("om_native_steer",
                "Change of direction: stop the earlier acceptance task after its current tool returns. "
                "Do not send Feishu messages or edit files. Incorporate this instruction and finish your turn normally. "
                "Use no lifecycle or bookkeeping tools. "
                "Only the native CLI should manage the turn."))
            def native_steer():
                return next((e for e in api(f"/projects/{key}/work")["events"] if e["source_id"] == "om_native_steer" and e["status"] in ("submitted","submitted","delivered")), None)
            steer = wait_for(native_steer, "new message was not submitted during the live native turn", timeout=8)
            submitted = time.time_ns() // 1_000_000
            work = api(f"/projects/{key}/work")
            original_run = next(r for r in work["runs"] if r["event_id"] == original["id"])
            assert original_run["finished_at"] is None, "original run finished before steering submission"
            def completed_native():
                work = api(f"/projects/{key}/work")
                return work if all(any(r["event_id"] == event_id and r["status"] == "settled" for r in work["runs"]) for event_id in (original["id"], steer["id"])) else None
            final = wait_for(completed_native, "native Agent did not incorporate steering and settle both runs", timeout=180)
            assert api(f"/projects/{key}")["project"]["generation"] == generation
            assert not FIXTURE.messages, "steering acceptance sent an unintended Feishu message"
            checks.append("native working turn receives steering and settles through HERDR without lifecycle tools or process restart")
            receipt = {"passed":True,"agent":real_agent,"checks":checks,"steering_submitted_ms":submitted,"original_run_unfinished_at_submission":True,"same_generation":True,"runs":[r for r in final["runs"] if r["event_id"] in (original["id"],steer["id"])],"run_dir":str(run)}
            (results / f"native-{real_agent}-steering.json").write_text(json.dumps(receipt, indent=2))
            print(f"Real {real_agent} busy-turn steering passed.")
            return
        FIXTURE.emit(message("om_first", "请先确认本消息，调用 send_md 发送一条带 **测试** 的消息，再调用 send_file 发送项目目录的 report.txt（send-file）。这是开发验证，只需完成这两项。"))
        wait_for(lambda: len(FIXTURE.messages) >= 2, "agent did not send Markdown and file",timeout=120 if real_agent else 35)
        assert FIXTURE.uploads and b"Fixture report" in FIXTURE.uploads[0]
        records = api(f"/projects/{key}")["messages"]
        assert next(m for m in records if m["source_id"] == "om_first")["status"] == "submitted"
        checks.append("WebSocket → durable inbox → PTY → native input → Markdown/file HTTP delivery")
        if real_agent:
            (results / f"real-{real_agent}.json").write_text(json.dumps({"passed": True,"checks": checks,"project":project["id"],"run_dir":str(run)},ensure_ascii=False,indent=2))
            print(f"Real {real_agent} model + MCP + HERDR + simulated Feishu round trip passed.")
            return
        def project_state():
            return api(f"/projects/{key}")["project"]

        def native_idle():
            if not real_agent:
                fixture_continuity()
            state = api("/state")
            terminal = next(p for p in state["projects"] if p["id"] == key)["terminal"]
            return state["runtime"]["terminals"].get(terminal, {}).get("status", {}).get("state") in {"idle", "done"}

        def settled():
            if not real_agent:
                fixture_continuity()
            work = api(f"/projects/{key}/work")
            return work if not any(r["status"] in {"submitting", "submitted", "running", "uncertain"} for r in work["runs"]) else None

        wait_for(settled, "initial run did not finish")
        second_workspace = run / "second-workspace"
        second_workspace.mkdir()
        second = api("/projects", {"name":"Second bot project","path":str(second_workspace),"agent":"custom","extra_args":extra_args})
        second_id = second["id"]
        assert next(p for p in api("/state")["projects"] if p["id"] == second_id)["bot"] is None
        try:
            api(f"/projects/{second_id}/bot", {"app_id":"cli_fixture","app_secret":"other-secret"}, "PUT")
            raise AssertionError("one bot was bound to two projects")
        except RuntimeError as error:
            assert "already connected to another project" in str(error)
        first_connection = next(c for c in FIXTURE.connections if FIXTURE.connection_bots[c] == "cli_fixture")
        api(f"/projects/{second_id}/bot", {"app_id":"cli_second","app_secret":"second-secret"}, "PUT")
        wait_for(lambda: "cli_second" in FIXTURE.connection_bots.values(), "second bot did not connect")
        assert first_connection in FIXTURE.connections, "connecting a second bot restarted the first"
        api(f"/projects/{second_id}/start", {}, "POST")
        wait_for(lambda: api(f"/projects/{second_id}")["project"]["ready"], "second Agent did not register MCP")
        for bot, marker in [("cli_fixture","project-a-only"), ("cli_second","project-b-only")]:
            FIXTURE.emit(message("om_shared_bot_test", marker), app_id=bot)
        for project_id, bot, marker, other_marker in [
            (key,"cli_fixture","project-a-only","project-b-only"),
            (second_id,"cli_second","project-b-only","project-a-only"),
        ]:
            wait_for(lambda bot=bot: any(m["fixture_app_id"] == bot and "om_shared_bot_test" in m["content"] for m in FIXTURE.messages), "bot did not reply with its own credentials")
            inbound = [m for m in api(f"/projects/{project_id}")["messages"] if m["source_id"] == "om_shared_bot_test"]
            assert len(inbound) == 1 and inbound[0]["content"] == marker
            reply = next(m for m in FIXTURE.messages if m["fixture_app_id"] == bot and "om_shared_bot_test" in m["content"])
            assert marker in reply["content"] and other_marker not in reply["content"]
            assert "测试用户-" + bot in reply["content"], "group context leaked between bots in the same chat"
        wait_for(settled, "first bot run did not finish")
        wait_for(lambda: not any(r["status"] in {"submitting","submitted","running","uncertain"} for r in api(f"/projects/{second_id}/work")["runs"]), "second bot run did not finish")
        old_second = next(c for c in FIXTURE.connections if FIXTURE.connection_bots[c] == "cli_second")
        FIXTURE.disconnect("cli_second")
        wait_for(lambda: any(FIXTURE.connection_bots[c] == "cli_second" and c != old_second for c in FIXTURE.connections), "second bot did not reconnect independently")
        assert first_connection in FIXTURE.connections
        api(f"/projects/{second_id}/stop", {}, "POST")
        api(f"/projects/{second_id}", method="DELETE")
        wait_for(lambda: "cli_second" not in FIXTURE.connection_bots.values(), "removed project's bot listener remained active")
        assert first_connection in FIXTURE.connections
        before_noise_ack = FIXTURE.acks
        noise = message("om_unrelated_group", "unrelated group chatter", addressed=False)
        FIXTURE.emit(noise)
        wait_for(lambda: FIXTURE.acks > before_noise_ack, "unrelated event was not acknowledged")
        assert not any(m["source_id"]=="om_unrelated_group" for m in api(f"/projects/{key}")["messages"])
        checks.append("two project bots isolate messages, tokens, same-group context and reconnects; new projects never inherit a bot")
        terminal = project_state()["terminal"]
        descriptor = api(f"/projects/{key}/show", {}, "POST")
        assert descriptor["terminal"] == terminal
        assert descriptor["session"] == session
        assert "launch_permit" not in descriptor.get("execution", {})
        view_env = dict(env, HERDR_CONFIG_PATH=config["herdr_config"])
        observer = subprocess.Popen([config["herdr"], "--session", session, "terminal", "session", "observe", terminal,
                                     "--cols", "100", "--rows", "30"], env=view_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(observer.stdout, selectors.EVENT_READ)
                assert selector.select(8), "terminal observer did not produce a frame"
                frame = json.loads(observer.stdout.readline())
            assert frame["type"] == "terminal.frame" and frame["encoding"] == "ansi"
            assert frame["width"] > 0 and frame["height"] > 0 and base64.b64decode(frame["bytes"])
        finally:
            observer.terminate(); observer.wait(timeout=5)
        assert project_state()["terminal"] == terminal and project_state()["ready"]
        checks.append("native terminal observe stream attaches and detaches without ending the Agent")
        before = len(FIXTURE.messages)
        FIXTURE.emit(message("om_first", "duplicate payload"))
        wait_for(lambda: FIXTURE.acks >= 2, "duplicate event did not receive SDK ack")
        assert len([m for m in api(f"/projects/{key}")["messages"] if m["source_id"] == "om_first"]) == 1
        assert len(FIXTURE.messages) == before
        checks.append("duplicate Feishu message is acknowledged without duplicate execution")
        for hold in ("fixture-hold-for-steer", "fixture-hold-unacknowledged"):
            wait_for(settled, "previous run did not settle before steering check")
            original_generation = project_state()["generation"]
            FIXTURE.emit(message("om_" + hold, hold))
            def held_run():
                work = api(f"/projects/{key}/work")
                original = next((e for e in work["events"] if e["source_id"] == "om_" + hold), None)
                native = legacy_rpc("pane.get", {"pane_id": project_state()["execution"]["pane_id"]})["pane"]
                return original and original["status"] == "submitted" and native["agent_status"] == "working"
            wait_for(held_run, "fixture did not hold the original run while working")
            for suffix, text in (("one", "Change course now"), ("two", "fixture-steer-release")):
                steer = message("om_steer_" + hold + suffix, text, addressed=False)
                steer["event"]["message"].update(thread_id="omt_om_first",root_id="om_first",parent_id="om_first")
                FIXTURE.emit(steer)
            wait_for(lambda: all(any(e["source_id"] == "om_steer_" + hold + suffix and e["status"] == "delivered" for e in api(f"/projects/{key}/work")["events"]) for suffix in ("one", "two")), "steering waited for the unfinished original run")
            wait_for(settled, "original run did not settle after steering")
            work = api(f"/projects/{key}/work")
            held_event = next(e for e in work["events"] if e["source_id"] == "om_" + hold)
            held = next(r for r in work["runs"] if r["event_id"] == held_event["id"])
            for suffix in ("one", "two"):
                steer = next(e for e in work["events"] if e["source_id"] == "om_steer_" + hold + suffix)
                assert steer["created_at"] <= held["finished_at"], "message did not steer the original run"
            assert project_state()["generation"] == original_generation
        checks.append("unmentioned replies in a verified topic steer the busy Agent before native turn completion without receipt/completion tools")
        FIXTURE.emit(message("om_second", "second message"))
        FIXTURE.emit(message("om_third", "", kind="file", content={"file_key": "fixture-inbound", "file_name": "incoming.txt"}))
        wait_for(lambda: all(any(m["source_id"] == key and m["status"] == "submitted" for m in api(f"/projects/{project['id']}")["messages"]) for key in ["om_second", "om_third"]), "queued messages failed")
        assert list((run / "files" / project["id"]).glob("*incoming.txt"))
        checks.append("ordered follow-up turns and streamed inbound attachment")
        rich_doc = {"title":"Screenshot", "content":[[{"tag":"text","text":"fixture-post-caption"},{"tag":"img","image_key":"img_fixture_post"},{"tag":"img","image_key":"img_fixture_post"}]]}
        FIXTURE.emit(message("om_rich_post", "", kind="post", content={"zh_cn":rich_doc,"en_us":rich_doc}))
        wait_for(lambda: (workspace / ".fixture-post-resource").exists(), "rich-post image did not reach the managed CLI envelope")
        rich_message = next(m for m in api(f"/projects/{project['id']}")["messages"] if m["source_id"]=="om_rich_post")
        assert rich_message["content"].count("fixture-post-caption")==1
        assert len(rich_message["attachment"]["resources"])==1
        assert rich_message["attachment"]["resources"][0]["local_path"]
        checks.append("localized rich-post caption and embedded image reach the native CLI once with a readable cached path")
        wait_for(settled, "rich post did not settle before resource steering check")
        FIXTURE.emit(message("om_slow_post", "", kind="post", content={"content":[[{"tag":"text","text":"slow resource"},{"tag":"img","image_key":"img_fixture_slow"}]]}))
        wait_for(FIXTURE.resource_started.is_set, "resource preparation did not start")
        FIXTURE.emit(message("om_after_slow_post", "steer while image is downloading"))
        def plain_passed_resource():
            deliveries=(workspace / ".fixture-deliveries").read_text()
            return '"source": "om_after_slow_post"' in deliveries
        wait_for(plain_passed_resource, "slow image preparation blocked later plain steering", timeout=10)
        slow_state=next(e for e in api(f"/projects/{key}/work")["events"] if e["source_id"]=="om_slow_post")
        assert slow_state["status"]=="queued", slow_state
        FIXTURE.resource_release.set()
        wait_for(lambda: any(m["source_id"]=="om_slow_post" and m["status"]=="submitted" for m in api(f"/projects/{key}")["messages"]), "prepared image did not enter normal delivery")
        checks.append("slow resource preparation leaves its message queued while later plain steering reaches the active Agent")
        FIXTURE.emit(message("om_private", "direct-to-user", chat_type="p2p", chat_id="oc_private"))
        wait_for(lambda: any(c["chat_id"]=="oc_private" for c in api("/conversations")["items"]), "new private chat was not discovered")
        assert next(p for p in api("/state")["projects"] if p["id"] == key)["bot"]["app_id"] == "cli_fixture"
        wait_for(lambda: any(m["receive_id"]=="ou_fixture" for m in FIXTURE.messages), "agent-selected user destination failed")
        wait_for(lambda: any(m["source_id"]=="om_private" and m["status"]=="submitted" for m in api(f"/projects/{key}")["messages"]), "private message did not join the same main session")
        checks.append("new private chat enters fixed bot project without chat binding + XML context + MCP explicit user to")
        FIXTURE.emit(message("om_new_group", "new group without binding", chat_id="oc_new_group"))
        wait_for(lambda: any(m["source_id"]=="om_new_group" and m["status"]=="submitted" for m in api(f"/projects/{key}")["messages"]), "new group did not automatically enter bot project")
        checks.append("new group automatically enters the same bot project without chat configuration")
        count = FIXTURE.connects; FIXTURE.disconnect()
        wait_for(lambda: FIXTURE.connects > count, "daemon did not reconnect WebSocket")
        checks.append("daemon supervises SDK and reconnects after forced disconnection")
        api(f"/projects/{key}/pause", {"paused": True})
        FIXTURE.emit(message("om_after_restart", "persist across daemon restart"))
        wait_for(lambda: any(m["source_id"] == "om_after_restart" for m in api(f"/projects/{key}")["messages"]), "paused event did not persist")
        old_terminal = api(f"/projects/{key}")["project"]["terminal"]
        daemon.terminate(); daemon.wait(timeout=15)
        daemon = boot()
        wait_for(lambda: api("/state"), "daemon did not restart")
        assert project_state()["terminal"] == old_terminal
        api(f"/projects/{key}/pause", {"paused": False})
        wait_for(lambda: any(m["source_id"] == "om_after_restart" and m["status"] == "submitted" for m in api(f"/projects/{key}")["messages"]), "persistent queue did not resume")
        wait_for(settled, "resumed run did not finish")
        checks.append("daemon restart preserves the live legacy execution and durable paused queue")

        # Only this fixture database's clock is advanced; production still uses ten minutes.
        def age_idle_execution():
            now = int(time.time() * 1000) - 601_000
            with sqlite3.connect(run / "bridge.sqlite3") as conn:
                conn.execute("UPDATE work_executions SET idle_since=?,last_activity=?,warm_until=? WHERE project_id=? AND status IN ('idle','running')",
                             (now, now, now + 600_000, key))

        wait_for(lambda: any(e["status"] == "settled" for e in api(f"/projects/{key}/work")["runs"]), "no completed run")
        wait_for(native_idle, "execution did not become idle")
        age_idle_execution()
        # A custom fixture (and an unsupported real CLI) has no native-child census.
        # Advancing a database clock must never manufacture safe child coverage.
        held_terminal = project_state()["terminal"]
        held_generation = project_state()["generation"]
        def unknown_retention_observed():
            with sqlite3.connect(run / "bridge.sqlite3") as conn:
                row = conn.execute("SELECT idle_since,warm_until FROM work_executions WHERE project_id=? AND generation=?", (key, held_generation)).fetchone()
                return row is not None and row == (None, None)
        # Observe an actual supervisor reconciliation before introducing new input,
        # which would itself reset eligibility and mask a retirement regression.
        wait_for(unknown_retention_observed, "unknown coverage did not clear the aged warm deadline")
        with sqlite3.connect(run / "bridge.sqlite3") as conn:
            assert conn.execute("SELECT COUNT(*) FROM idle_handoffs WHERE project_id=?", (key,)).fetchone()[0] == 0
        FIXTURE.emit(message("om_unknown_steer", "steer retained native session"))
        wait_for(lambda: any(e["source_id"] == "om_unknown_steer" and e["status"] == "delivered" for e in api(f"/projects/{key}/work")["events"]), "unknown child coverage blocked steering")
        wait_for(settled, "retained session steering did not settle")
        assert project_state()["terminal"] == held_terminal
        assert project_state()["generation"] == held_generation
        with sqlite3.connect(run / "bridge.sqlite3") as conn:
            assert conn.execute("SELECT COUNT(*) FROM idle_handoffs WHERE project_id=?", (key,)).fetchone()[0] == 0
        checks.append("unknown native-child coverage retains the execution beyond the idle deadline while steering still works")

        # Explicit stop still preserves native continuity and timer wakeup.
        api(f"/projects/{key}/stop", {})
        wait_for(lambda: project_state()["terminal"] is None, "explicit stop did not release owned execution")
        due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)
        timer = api(f"/projects/{key}/timers", {"name": "Scheduled wakeup", "prompt": "scheduled-wakeup", "kind": "once",
                    "at": due.isoformat(), "timezone": "UTC", "to": "oc_fixture", "request_id": "fixture-timer"})
        wait_for(lambda: "scheduled-wakeup" in str(FIXTURE.messages), "timer did not wake the fixture and deliver its result")
        wait_for(settled, "timer run did not finish")
        assert project_state()["terminal"] is not None
        assert len(legacy_rpc("workspace.list")["workspaces"]) == 1, "timer wake leaked a workspace"
        checks.append("durable one-shot timer wakes an explicitly stopped fixture through the persisted input journal")
        FIXTURE.emit(message("om_schedule_tool", "fixture-create-schedule"))
        wait_for(lambda: "scheduled-via-mcp" in str(FIXTURE.messages), "schedule_run tool did not persist and fire its schedule")
        wait_for(settled, "MCP scheduled input did not settle through native idle")
        assert next(t for t in api(f"/projects/{key}/timers") if t["name"] == "MCP schedule")["purpose"] == "user"
        checks.append("schedule_run creates a durable one-shot through real MCP and native idle releases its occurrence")
        FIXTURE.emit(message("om_internal_schedule", "fixture-create-internal-schedule"))
        wait_for(lambda: "scheduled-internal-via-mcp" in str(FIXTURE.messages), "internal schedule did not execute")
        wait_for(settled, "internal scheduled input did not settle through native idle")
        internal = [t for t in api(f"/projects/{key}/timers") if t["purpose"] == "internal"]
        assert len(internal) == 1 and internal[0]["next_fire"] is None, internal
        events = api(f"/projects/{key}/work")["events"]
        assert len([e for e in events if e["payload"].get("timer_id") == internal[0]["id"]]) == 1
        checks.append("explicit internal MCP one-shot executes and deduplicates retries while introspection preserves its purpose and occurrence")

        api(f"/projects/{key}/pause", {"paused":True})
        due = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=120)
        recurring = api(f"/projects/{key}/timers", {"name":"Recover interval", "prompt":"interval-after-restart", "kind":"interval",
                    "at":due.isoformat(), "interval_seconds":60, "timezone":"UTC", "to":"oc_fixture", "request_id":"recover-interval"})
        def interval_events():
            return [e for e in api(f"/projects/{key}/work")["events"] if e["payload"].get("timer_id") == recurring["id"]]
        wait_for(lambda: len(interval_events()) == 1, "missed intervals did not coalesce")
        daemon.terminate(); daemon.wait(timeout=15)
        daemon = boot()
        wait_for(lambda: api("/state"), "daemon did not recover interval schedule")
        assert len(interval_events()) == 1, "restart duplicated a stored occurrence"
        api(f"/projects/{key}/pause", {"paused":False})
        wait_for(lambda: "interval-after-restart" in str(FIXTURE.messages), "stored interval did not deliver after restart")
        wait_for(settled, "recovered interval did not settle")
        assert api(f"/timers/{recurring['id']}/pause", {"enabled":False})["enabled"] is False
        assert api(f"/timers/{recurring['id']}/pause", {"enabled":True})["enabled"] is True
        api(f"/timers/{recurring['id']}", method="DELETE")
        disabled = next(t for t in api(f"/projects/{key}/timers") if t["id"] == recurring["id"])
        assert not disabled["enabled"] and disabled["next_fire"] is None
        checks.append("interval recovery coalesces persisted occurrences; native timer pause/enable/delete paths survive restart")

        cli = subprocess.run([str(binary), "--data-dir", str(run), "project", "list"], capture_output=True, text=True, timeout=10)
        assert cli.returncode == 0 and "端到端验证项目" in cli.stdout
        checks.append("CLI and UI API share the same project state")
        # A user-added terminal in the same workspace must survive project stop.
        user_tab = legacy_rpc("layout.apply", {"workspace_id": project_state()["execution"]["workspace_id"],
                    "focus": False, "root": {"type": "pane", "cwd": str(workspace),
                    "command": [sys.executable, "-c", "import time; time.sleep(120)"], "label": "Unrelated fixture terminal"}})
        user_pane = user_tab["layout"]["root"]["pane_id"]
        user_identity = legacy_rpc("pane.get", {"pane_id": user_pane})["pane"]["terminal_id"]
        # Interrupt an acknowledged run, then prove fresh input still wakes the
        # same native conversation without replaying the interrupted event.
        FIXTURE.emit(message("om_interrupted", "fixture-hold-for-steer"))
        wait_for(lambda: any(e["source_id"] == "om_interrupted" and e["status"] == "submitted" for e in api(f"/projects/{key}/work")["events"]), "fixture did not begin the interrupted run")
        stopped_generation = project_state()["generation"]
        if not real_agent:
            fixture_continuity()
        stopped_native = project_state()["native_session"]
        assert stopped_native, "explicit recovery requires a known native session"
        api(f"/projects/{key}/stop", {}, "POST")
        assert legacy_rpc("pane.get", {"pane_id": user_pane})["pane"]["terminal_id"] == user_identity
        assert len(legacy_rpc("workspace.list")["workspaces"]) == 1
        checks.append("project stop preserves unrelated user-added terminals in its workspace")
        legacy_rpc("pane.close", {"pane_id": user_pane})
        wait_for(lambda: next(p for p in api("/state")["projects"] if p["id"] == key)["lifecycle"] == "sleeping", "interrupted history incorrectly made a dormant project require manual startup")
        FIXTURE.emit(message("om_after_stop", "wake automatically after stop"))
        wait_for(lambda: any(e["source_id"] == "om_after_stop" and e["status"] == "delivered" for e in api(f"/projects/{key}/work")["events"]), "stopped execution did not automatically wake for new input")
        assert project_state()["generation"] != stopped_generation
        assert project_state()["native_session"] == stopped_native
        if not real_agent:
            launches = [json.loads(line) for line in (workspace / ".fixture-launches").read_text().splitlines()]
            assert launches[-1]["resume"] == stopped_native
        assert next(e for e in api(f"/projects/{key}/work")["events"] if e["source_id"] == "om_interrupted")["status"] == "uncertain"
        checks.append("new messages automatically resume a stopped execution without replaying its uncertain interrupted work")
        api(f"/projects/{key}/pause", {"paused": True})
        api(f"/projects/{key}/stop", {}, "POST")
        FIXTURE.emit(message("om_explicit_pause", "preserve an explicit pause"))
        wait_for(lambda: any(e["source_id"] == "om_explicit_pause" and e["status"] == "queued" for e in api(f"/projects/{key}/work")["events"]), "explicitly paused message did not persist")
        assert project_state()["terminal"] is None
        api(f"/projects/{key}/pause", {"paused": False})
        wait_for(lambda: any(e["source_id"] == "om_explicit_pause" and e["status"] == "delivered" for e in api(f"/projects/{key}/work")["events"]), "unpause did not automatically resume delivery")
        checks.append("only an explicit pause holds new messages; resuming activity automatically wakes the Agent")
        wait_for(native_idle, "old uncertain work pinned the new execution")
        before_final_stop = project_state()["native_session"]
        assert before_final_stop
        api(f"/projects/{key}/stop", {}, "POST")
        wait_for(lambda: project_state()["terminal"] is None, "new execution could not stop with old uncertain history")
        due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)
        api(f"/projects/{key}/timers", {"name":"Independent scheduled work","prompt":"independent-after-stop","kind":"once","at":due.isoformat(),"timezone":"UTC","to":"oc_fixture"})
        wait_for(lambda: "independent-after-stop" in str(FIXTURE.messages), "old uncertain history blocked independent scheduled wakeup")
        if not real_agent:
            fixture_continuity()
            assert project_state()["native_session"] == before_final_stop
            launches = [json.loads(line) for line in (workspace / ".fixture-launches").read_text().splitlines()]
            assert launches[-1]["resume"] == before_final_stop
        assert next(e for e in api(f"/projects/{key}/work")["events"] if e["source_id"] == "om_interrupted")["status"] == "uncertain"
        checks.append("old uncertain history stays recorded without pinning a new execution or blocking independent timer wakeup")
        (results / "smoke.json").write_text(json.dumps({"passed": True, "checks": checks, "run_dir": str(run)}, ensure_ascii=False, indent=2))
        print(json.dumps({"passed": True, "checks": checks, "run_dir": str(run)}, ensure_ascii=False, indent=2))
    except Exception:
        if "key" in locals():
            try:
                diagnostic = run / "failure-screen.json"
                diagnostic.write_text(json.dumps({"state":api("/state"),"screen":api(f"/projects/{key}/screen")}, ensure_ascii=False, indent=2))
                diagnostic.chmod(0o600)
            except Exception:
                pass
        raise
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate(); daemon.wait(timeout=15)
        if daemon is not None:
            subprocess.run([config["herdr"], "--session", session, "server", "stop"], env=env, capture_output=True, timeout=15)
        FIXTURE.disconnect(); FIXTURE.server.shutdown(); log.close()
        runtime_dir.cleanup()
        if real_agent == "agy":
            if agy_original is None:
                agy_config.unlink(missing_ok=True)
            else:
                agy_config.write_bytes(agy_original)
                agy_config.chmod(0o600)


if __name__ == "__main__":
    main()
