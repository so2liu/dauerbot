"""A deterministic protocol fixture: real legacy HERDR PTY, MCP, and daemon.

Its persisted marker checks fixture continuity, not any model's native resume.
"""
import json
import datetime
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import tty
import xml.etree.ElementTree as ET

tty.setraw(0)
sys.stdout.write("\x1b[?2004h")
sys.stdout.flush()


def status(state):
    request = {"id": "fixture-status", "method": "pane.report_agent", "params": {
        "pane_id": os.environ["HERDR_PANE_ID"], "source": "dauerbot-fixture",
        "agent": "fixture-agent", "state": state, "seq": time.time_ns(),
    }}
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(3)
        conn.connect(os.environ["HERDR_SOCKET_PATH"])
        conn.sendall((json.dumps(request) + "\n").encode())
        result = json.loads(conn.makefile().readline())
        if "error" in result:
            raise RuntimeError(result["error"])


status("idle")
resolved_herdr = shutil.which("herdr")
assert resolved_herdr, "Native Agent tools cannot find HERDR in PATH"
assert subprocess.check_output([resolved_herdr, "--version"], text=True, timeout=5).strip() == "herdr 0.9.5"
(Path.cwd() / ".fixture-herdr").write_text(resolved_herdr)
marker = Path.cwd() / ".fixture-session"
expected = sys.argv[3] if len(sys.argv) > 3 else ""
if marker.exists():
    assert expected == marker.read_text(), "Fixture resume did not retain native identity"
else:
    assert not expected
    marker.write_text("fixture-native-session")
proc = subprocess.Popen([sys.argv[1], "mcp", "--credentials", sys.argv[2]],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
sequence = 0


def rpc(method, params=None):
    global sequence
    sequence += 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence, "method": method, "params": params or {}}) + "\n")
    proc.stdin.flush()
    for line in proc.stdout:
        value = json.loads(line)
        if value.get("id") == sequence:
            if "error" in value:
                raise RuntimeError(value["error"])
            return value["result"]
    raise RuntimeError("MCP closed unexpectedly")


rpc("initialize", {"protocolVersion": "2025-06-18", "clientInfo": {"name": "hfb-fixture", "version": "2"}, "capabilities": {}})
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
proc.stdin.flush()
tools = rpc("tools/list")
assert all("to" in t["inputSchema"]["required"] for t in tools["tools"] if t["name"] in {"send_md", "send_file"})
assert {t["name"] for t in tools["tools"]} == {"send_md", "send_file", "schedule_run"}


def tool(name, arguments):
    result = rpc("tools/call", {"name": name, "arguments": arguments})
    if result.get("isError"):
        raise RuntimeError(result)
    return result


sys.stdout.write("\r\nMOCK_AGENT_READY\r\n")
sys.stdout.flush()
buffer = b""
held = None
try:
    while True:
        buffer += os.read(0, 65536)
        end = b"\x1b[201~\r"
        while end in buffer:
            text, buffer = buffer.split(end, 1)
            text = text.removeprefix(b"\x1b[200~").decode()
            event = ET.fromstring(text)
            assert event.tag == "dauerbot_event", event.tag
            status("working")
            body = event.findtext("feishu_message/body") or event.findtext("body") or ""
            if body in ("fixture-hold-unacknowledged", "fixture-hold-for-steer"):
                assert held is None
                held = event
                sys.stdout.write("\r\nMOCK_HOLDING_FOR_STEER\r\n")
                sys.stdout.flush()
                continue
            envelope = event.find("feishu_message")
            if envelope is not None:
                source = envelope.attrib["message_id"]
                to = envelope.find("source").attrib["chat_id"] or "oc_fixture"
                if "direct-to-user" in text:
                    to = envelope.find("sender").attrib["open_id"]
            else:
                source = event.attrib["source"] + ":" + event.attrib["event_id"]
                to = "oc_fixture"
            tool("send_md", {"to": to, "text": f"**Fixture result**\n\n{source}\n\n{text}", "request_id": "reply-" + source})
            if body == "fixture-create-schedule":
                due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)
                tool("schedule_run", {"request_id":"fixture-schedule-run", "name":"MCP schedule", "prompt":"scheduled-via-mcp",
                     "kind":"once", "at":due.isoformat(), "timezone":"UTC", "to":to})
            if "send-file" in text:
                tool("send_file", {"to": to, "path": str(Path.cwd() / "report.txt"), "request_id": "file-" + source})
            if body == "fixture-steer-release":
                assert held is not None, "Steering arrived without a live original run"
                held = None
            sys.stdout.write("\r\nMOCK_PROCESSED " + json.dumps({"source": source, "pid": os.getpid()}, ensure_ascii=False) + "\r\n")
            sys.stdout.flush()
            status("working" if held is not None else "idle")
finally:
    proc.terminate()
    proc.wait(timeout=5)
