"""A protocol fixture, not a model: real PTY + real MCP stdio + bridge HTTP."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tty

tty.setraw(0)
sys.stdout.write("\x1b[?2004h")


def status(state):
    sys.stdout.write(f"\x1b]7501;state={state}:app=hfb-test-agent\x1b\\")
    sys.stdout.flush()


status("idle")
proc = subprocess.Popen(
    [sys.argv[1], "mcp", "--credentials", sys.argv[2]],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
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


rpc("initialize", {"protocolVersion": "2025-06-18", "clientInfo": {"name": "hfb-fixture", "version": "1"}, "capabilities": {}})
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
proc.stdin.flush()
tools = rpc("tools/list")
assert {t["name"] for t in tools["tools"]} == {"session_ready", "ack_message", "send_markdown", "send_file"}, tools


def tool(name, arguments):
    result = rpc("tools/call", {"name": name, "arguments": arguments})
    if result.get("isError"):
        raise RuntimeError(result)
    return result


tool("session_ready", {"native_session": "fixture-native-session"})
sys.stdout.write("\r\nMOCK_AGENT_READY\r\n")
sys.stdout.flush()
buffer = b""
try:
    while True:
        buffer += os.read(0, 4096)
        end = b"\x1b[201~\r"
        if end not in buffer:
            continue
        text, buffer = buffer.split(end, 1)
        text = text.removeprefix(b"\x1b[200~").decode()
        source = re.search(r"\[FEISHU_MESSAGE id=([^\]]+)\]", text).group(1)
        status("working")
        tool("ack_message", {"message_id": source})
        tool("send_markdown", {"text": f"**收到测试消息**\n\n{source}\n\n中文与 Markdown 均已往返。", "request_id": "reply-" + source})
        if "send-file" in text:
            tool("send_file", {"path": str(Path.cwd() / "report.txt"), "request_id": "file-" + source})
        sys.stdout.write("\r\nMOCK_PROCESSED " + json.dumps({"source": source, "pid": os.getpid()}, ensure_ascii=False) + "\r\n")
        status("done")
        status("idle")
finally:
    proc.terminate()
