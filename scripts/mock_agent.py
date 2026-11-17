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
        "agent": sys.argv[5] if len(sys.argv)>5 else "fixture-agent", "state": state, "seq": time.time_ns(),
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
if expected:
    assert expected == marker.read_text(), "Fixture resume did not retain native identity"
else:
    import uuid
    marker.write_text(str(uuid.uuid4()))
with (Path.cwd() / ".fixture-launches").open("a") as launches:
    launches.write(json.dumps({"resume": expected, "native": marker.read_text(), "generation": Path(sys.argv[2]).parent.name, "instructions": sys.argv[4] if len(sys.argv) > 4 else ""}) + "\n")
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
            if event.tag == "dauerbot_cli_switch":
                status("working")
                mode = Path.cwd() / ".fixture-switch-mode"
                if mode.exists() and mode.read_text() == "hold":
                    (Path.cwd() / ".fixture-switch-held").write_text(event.attrib["request_id"])
                    continue
                import re
                nonce = re.search(r"dauerbot-switch-ready:([a-zA-Z0-9-]+)", event.text).group(1)
                text = "# CLI handoff\n\nAll native children and external workers have been settled. Continue the current fixture goal from this workspace. Preserve completed effects and never replay earlier replies.\n"
                if not mode.exists() or mode.read_text() != "stale":
                    text += "<!-- dauerbot-switch-ready:" + nonce + " -->\n"
                (Path.cwd() / "HANDOFF.md").write_text(text)
                status("idle")
                continue
            if event.tag == "dauerbot_handoff":
                status("working")
                request = event.attrib["request_id"]
                mode = Path.cwd() / ".fixture-handoff-mode"
                if mode.exists() and mode.read_text() == "hold":
                    sys.stdout.write("\r\nMOCK_HANDOFF_HELD\r\n")
                    sys.stdout.flush()
                    continue
                text = ("# Handoff\n\nGoal: continue the fixture project.\nCompleted work: handled messages.\nKey decisions: use native sessions.\nRelevant files/commits: report.txt.\nPending issues: none.\nNext actions: process the next message.\n")
                if not mode.exists() or mode.read_text() != "stale":
                    text += "<!-- dauerbot-handoff:" + request + " -->\n"
                (Path.cwd() / "HANDOFF.md").write_text(text)
                status("idle")
                continue
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
            reply_anchor = {"reply_to_message_id": source} if envelope is not None and to == envelope.find("source").attrib["chat_id"] else {}
            tool("send_md", {"to": to, "text": f"**Fixture result**\n\n{source}\n\n{text}", "request_id": "reply-" + source, **reply_anchor})
            if body == "fixture-create-schedule":
                due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)
                tool("schedule_run", {"request_id":"fixture-schedule-run", "name":"MCP schedule", "prompt":"scheduled-via-mcp",
                     "kind":"once", "at":due.isoformat(), "timezone":"UTC", "to":to, "purpose":"user"})
            if body == "fixture-create-internal-schedule":
                due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)
                scheduled = {"request_id":"fixture-internal-run", "name":"Internal follow-up", "prompt":"scheduled-internal-via-mcp",
                             "kind":"once", "at":due.isoformat(), "timezone":"UTC", "to":to, "purpose":"internal"}
                tool("schedule_run", scheduled)
                tool("schedule_run", scheduled)
            if "send-file" in text:
                tool("send_file", {"to": to, "path": str(Path.cwd() / "report.txt"), "request_id": "file-" + source, **reply_anchor})
            if body == "fixture-steer-release":
                assert held is not None, "Steering arrived without a live original run"
                held = None
            sys.stdout.write("\r\nMOCK_PROCESSED " + json.dumps({"source": source, "pid": os.getpid()}, ensure_ascii=False) + "\r\n")
            sys.stdout.flush()
            status("working" if held is not None else "idle")
finally:
    proc.terminate()
    proc.wait(timeout=5)
