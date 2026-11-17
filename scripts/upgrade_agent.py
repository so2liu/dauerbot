"""Cross-version test Agent. Calls each executable's actual published MCP contract."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import tty
import uuid
import xml.etree.ElementTree as ET

tty.setraw(0)
sys.stdout.write("\x1b[?2004h")
sys.stdout.flush()
workspace = Path.cwd()
marker = workspace / ".upgrade-native"
expected = sys.argv[3]
if expected:
    assert marker.read_text() == expected, "Native continuity was lost"
else:
    marker.write_text(str(uuid.uuid4()))
native = marker.read_text()


def status(state):
    request = {"id": "upgrade-status", "method": "pane.report_agent", "params": {
        "pane_id": os.environ["HERDR_PANE_ID"], "source": "upgrade-fixture",
        "agent": "upgrade-fixture", "state": state, "seq": time.time_ns(),
        "agent_session_id": native,
    }}
    with socket.socket(socket.AF_UNIX) as conn:
        conn.connect(os.environ["HERDR_SOCKET_PATH"])
        conn.sendall((json.dumps(request) + "\n").encode())
        response = json.loads(conn.makefile().readline())
        assert "error" not in response, response


status("idle")
proc = subprocess.Popen([sys.argv[1], "mcp", "--credentials", sys.argv[2]],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
sequence = 0


def rpc(method, params=None):
    global sequence
    sequence += 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence, "method": method, "params": params or {}}) + "\n")
    proc.stdin.flush()
    for line in proc.stdout:
        result = json.loads(line)
        if result.get("id") == sequence:
            assert "error" not in result, result
            return result["result"]
    raise RuntimeError("MCP exited")


def tool(name, arguments):
    result = rpc("tools/call", {"name": name, "arguments": arguments})
    assert not result.get("isError"), result


rpc("initialize", {"protocolVersion": "2025-06-18", "clientInfo": {"name": "upgrade-fixture", "version": "1"}, "capabilities": {}})
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
proc.stdin.flush()
names = {t["name"] for t in rpc("tools/list")["tools"]}
legacy = "session_ready" in names
if legacy:
    tool("session_ready", {"native_session": native})
else:
    assert names == {"send_md", "send_file", "schedule_run"}, names
with (workspace / "launches.jsonl").open("a") as out:
    out.write(json.dumps({"native": native, "resume": expected, "legacy": legacy,
                          "pid": os.getpid(), "mcp_pid": proc.pid, "command": sys.argv[1],
                          "generation": Path(sys.argv[2]).parent.name}) + "\n")
buffer = b""
try:
    while True:
        buffer += os.read(0, 65536)
        while b"\x1b[201~\r" in buffer:
            raw, buffer = buffer.split(b"\x1b[201~\r", 1)
            event = ET.fromstring(raw.removeprefix(b"\x1b[200~").decode())
            status("working")
            body = event.findtext("feishu_message/body") or event.findtext("body") or ""
            with (workspace / "received.jsonl").open("a") as out:
                out.write(json.dumps({"body": body, "event": event.attrib["event_id"]}) + "\n")
            if body == "hold-before-upgrade":
                continue
            if legacy:
                tool("ack_event", {"event_id": event.attrib["event_id"]})
            tool("send_markdown" if legacy else "send_md",
                 {"to": "oc_fixture", "text": body, "request_id": "reply-" + event.attrib["event_id"]})
            if legacy:
                tool("finish_run", {"run_id": event.attrib["run_id"], "outcome": "completed", "summary": "Fixture result sent"})
            status("idle")
finally:
    proc.terminate()
    proc.wait(timeout=5)
