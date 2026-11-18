"""A deterministic protocol fixture: real legacy HERDR PTY, MCP, and daemon.

Its persisted marker checks fixture continuity, not any model's native resume.
"""
import json
import datetime
import os
import select
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import threading
import tty
import xml.etree.ElementTree as ET

tty.setraw(0)
sys.stdout.write("\x1b[?2004h")
sys.stdout.flush()


native_session_value = None


def status(state):
    native_hook = len(sys.argv) > 6 and json.loads(sys.argv[6]).get("native_session_hook")
    if native_hook == "pi":
        if native_session_value is None:
            return
        request = {"id":"fixture-pi-state","method":"pane.report_agent","params":{
            "pane_id":os.environ["HERDR_PANE_ID"],"source":"herdr:pi","agent":"pi",
            "state":state,"seq":time.time_ns(),"agent_session_path":native_session_value}}
    elif native_hook:
        # HERDR 0.9.5 reserves herdr:codex state reports for identity only;
        # a different hook source cannot own an officially anchored session.
        # Exercise its actual Codex OSC-title detector instead. Wait for every
        # transition so a fast working/idle pair cannot collapse into one poll.
        if not os.environ.get("CODEX_THREAD_ID"):
            return
        title = "⠋ Fixture working" if state == "working" else "Fixture idle"
        sys.stdout.write("\x1b]0;" + title + "\x07")
        sys.stdout.flush()
        deadline = time.monotonic() + 8
        observed = None
        while time.monotonic() < deadline:
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(3)
                conn.connect(os.environ["HERDR_SOCKET_PATH"])
                conn.sendall((json.dumps({"id":"fixture-native-state", "method":"agent.get", "params":{"target":os.environ["HERDR_PANE_ID"]}}) + "\n").encode())
                result = json.loads(conn.makefile().readline())
            if "error" not in result:
                observed = result.get("result", {}).get("agent", {}).get("agent_status")
                if observed == state or state == "idle" and observed == "done":
                    return
            time.sleep(0.05)
        (Path.cwd() / ".fixture-native-state-error").write_text(json.dumps({"requested":state,"observed":observed,"reply":result}))
        raise RuntimeError(f"Native Codex OSC state did not become {state}: {observed}")
    else:
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
# The target shim must not overwrite the source fixture's resumable identity.
marker = Path.cwd() / (".fixture-session-codex" if len(sys.argv) > 5 and sys.argv[5] == "codex" else ".fixture-session")
expected = sys.argv[3] if len(sys.argv) > 3 else ""
if expected:
    assert expected == marker.read_text(), "Fixture resume did not retain native identity"
else:
    import uuid
    marker.write_text(str(uuid.uuid4()))
launch_metadata = json.loads(sys.argv[6]) if len(sys.argv) > 6 else {}
if launch_metadata.get("native_session_hook"):
    agent = sys.argv[5]
    assert agent in {"codex", "pi"}
    if agent == "pi":
        # A real Pi TUI renders before hooks/MCP initialize; output also wakes
        # HERDR's native foreground acquisition rather than inventing an Agent.
        sys.stdout.write("\r\nPi fixture 1.5.1\r\n")
        sys.stdout.flush()
    if agent == "pi" and not expected:
        if launch_metadata.get("assigned_native_id"):
            marker.write_text(launch_metadata["assigned_native_id"])
        native_file = Path.cwd() / (".fixture-native-" + marker.read_text() + ".jsonl")
        native_file.write_text(json.dumps({"type":"session","version":3,"id":marker.read_text(),"cwd":str(Path.cwd())}) + "\n")
        marker.write_text(str(native_file))
    native_session_value = marker.read_text()
    fork = Path.cwd() / ".fixture-fork-on-resume"
    if expected and fork.exists():
        # Default remains the original one-shot fixture. The reboot recovery
        # regression opts into a persistent fork that only its repair Agent clears.
        if fork.read_text().strip() != "persistent":
            fork.unlink()
        import uuid
        observed = str(uuid.uuid4())
        if agent == "pi":
            file = Path.cwd() / (".fixture-fork-" + observed + ".jsonl")
            file.write_text(json.dumps({"type":"session","version":3,"id":observed}) + "\n")
            observed = str(file)
        native_session_value = observed
        fork_record = {"expected":expected,"observed":observed,"generation":Path(sys.argv[2]).parent.name,
                       "pane":os.environ["HERDR_PANE_ID"],"credentials":sys.argv[2]}
        (Path.cwd() / ".fixture-native-fork").write_text(json.dumps(fork_record))
        with (Path.cwd() / ".fixture-native-forks").open("a") as forks:
            forks.write(json.dumps(fork_record) + "\n")
    params = {"pane_id":os.environ["HERDR_PANE_ID"],"source":"herdr:" + agent,"agent":agent,
              "seq":time.time_ns(),"session_start_source":"resume" if expected else ("new" if agent == "pi" else "startup")}
    params["agent_session_path" if agent == "pi" else "agent_session_id"] = native_session_value
    if agent == "codex":
        os.environ["CODEX_THREAD_ID"] = native_session_value
    request = {"id":"fixture-native-session","method":"pane.report_agent_session","params":params}
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(3)
        conn.connect(os.environ["HERDR_SOCKET_PATH"])
        conn.sendall((json.dumps(request) + "\n").encode())
        result = json.loads(conn.makefile().readline())
        if "error" in result:
            raise RuntimeError(result["error"])
    if agent == "pi":
        adapter = Path(launch_metadata["child_adapter"])
        evidence = {"schema":1,"generation":Path(sys.argv[2]).parent.name,"session":native_session_value,
                    "sessionFile":native_session_value,"epoch":"fixture","revision":1,"state":"quiescent","updated":int(time.time()*1000)}
        def publish_children():
            evidence["updated"] = int(time.time()*1000)
            temp = adapter / "fixture-children.tmp"
            temp.write_text(json.dumps(evidence))
            temp.replace(adapter / "children.json")
        publish_children()
        def child_heartbeat():
            while True:
                time.sleep(1.5)
                publish_children()
        # The bundled managed.ts publishes the same 1500ms heartbeat.
        threading.Thread(target=child_heartbeat, daemon=True).start()
    else:
        sys.stdout.write("\r\n› Ask Codex to do anything\r\n")
        sys.stdout.flush()
    status("idle")
    if launch_metadata.get("native_state_probe"):
        status("working")
        status("idle")
with (Path.cwd() / ".fixture-launches").open("a") as launches:
    launch = {"resume": expected, "native": native_session_value or marker.read_text(), "generation": Path(sys.argv[2]).parent.name, "instructions": sys.argv[4] if len(sys.argv) > 4 else ""}
    if len(sys.argv) > 6:
        launch["launch"] = json.loads(sys.argv[6])
    launches.write(json.dumps(launch) + "\n")
while (Path.cwd() / ".fixture-mcp-hold").exists():
    time.sleep(0.05)
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


initialization = rpc("initialize", {"protocolVersion": "2025-06-18", "clientInfo": {"name": "hfb-fixture", "version": "2"}, "capabilities": {}})
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
switch_held = None
repair_held = None
repair_lane = None


def switch_handoff(event):
    mode = Path.cwd() / ".fixture-switch-mode"
    nonce = event.attrib["handoff_nonce"]
    text = "# CLI handoff\n\nAll native children and external workers have been settled. Continue the current fixture goal from this workspace. Preserve completed effects and never replay earlier replies.\n"
    if not mode.exists() or mode.read_text() != "stale":
        text += "<!-- dauerbot-switch-ready:" + nonce + " -->\n"
    (Path.cwd() / "HANDOFF.md").write_text(text)
    status("idle")


def repair_handoff(event):
    # Only fixture files are repaired. A real CLI/model is never invoked here.
    (Path.cwd() / ".fixture-target-exit").unlink(missing_ok=True)
    (Path.cwd() / ".fixture-switch-mode").unlink(missing_ok=True)
    handoff = event.find("dauerbot_cli_switch")
    assert handoff is not None, "Repair control did not contain the fresh handoff request"
    switch_handoff(handoff)


def native_repair_control(event, wrapped):
    global repair_lane
    if event.tag == "dauerbot_cli_repair" and event.attrib.get("repair_only") == "true":
        assert repair_lane is None, "Repair lane received duplicate initial prompt"
        assert event.attrib["original_native"] == marker.read_text()
        assert event.attrib["observed_native"] == native_session_value != marker.read_text()
        instructions = initialization.get("instructions", "")
        assert "repair-only" in instructions.lower() and marker.read_text() in instructions, initialization
        repair_lane = event
        generation = Path(sys.argv[2]).parent.name
        diagnostic = tool("send_md", {"to":"oc_fixture", "text":"Fixture repair-only Agent is investigating preserved native continuity.",
            "request_id":"repair-lane-diagnostic-" + generation, "reply_to_message_id":"om_reboot_warm_0"})
        due = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
        denied = rpc("tools/call", {"name":"schedule_run", "arguments":{"request_id":"repair-lane-denied-" + generation,
            "name":"Must be denied", "prompt":"repair lane must not schedule ordinary work", "kind":"once", "at":due.isoformat(),
            "timezone":"UTC", "to":"oc_fixture", "purpose":"internal"}})
        assert denied.get("isError") and "repair" in json.dumps(denied).lower(), denied
        (Path.cwd() / ".fixture-repair-lane-ready").write_text(json.dumps({"generation":generation,
            "original":marker.read_text(),"observed":native_session_value,"request":event.attrib["request_id"],
            "nonce":event.find("dauerbot_cli_switch").attrib["handoff_nonce"],
            "diagnostic":diagnostic,"schedule_denied":denied}))
    if repair_lane is None:
        return False
    instructions = []
    for element in wrapped.findall("repair_user_instructions"):
        assert element.attrib.get("trust") == "user_input"
        instructions.extend(json.loads(element.text or "[]"))
    controls = [item for item in instructions if item.get("content") == "fixture-repair-persistent-native-resume"]
    if not controls:
        status("idle")
        return True
    assert len(controls) == 1, controls
    control = controls[0]
    assert control["message_id"] == "om_reboot_repair_0", control
    assert control["source"]["chat_id"] == "oc_fixture" and control["source"]["sender"]["open_id"] == "ou_fixture", control
    tool("send_md", {"to":"oc_fixture", "text":"Fixture repair-only Agent received trusted repair instructions; restoring the saved conversation.",
        "request_id":"repair-lane-control-" + Path(sys.argv[2]).parent.name, "reply_to_message_id":control["message_id"]})
    # This changes only the fixture's private simulated CLI failure. Saved A and
    # the daemon journal remain untouched by the diagnostic Agent.
    (Path.cwd() / ".fixture-fork-on-resume").unlink()
    handoff = event.find("dauerbot_cli_switch")
    if handoff is None:
        handoff = repair_lane.find("dauerbot_cli_switch")
    assert handoff is not None
    (Path.cwd() / ".fixture-native-repaired").write_text(json.dumps({"generation":Path(sys.argv[2]).parent.name,
        "original":marker.read_text(),"observed":native_session_value,"control":control,"nonce":handoff.attrib["handoff_nonce"]}))
    switch_handoff(handoff)
    return True


try:
    while True:
        repair_release = Path.cwd() / ".fixture-repair-release"
        if repair_held is not None and repair_release.exists():
            repair_release.unlink()
            repair_handoff(repair_held)
            repair_held = None
        release = Path.cwd() / ".fixture-switch-release"
        if switch_held is not None and release.exists():
            release.unlink()
            switch_handoff(switch_held)
            switch_held = None
        if not select.select([0], [], [], 0.05)[0]:
            continue
        buffer += os.read(0, 65536)
        end = b"\x1b[201~\r"
        while end in buffer:
            text, buffer = buffer.split(end, 1)
            text = text.removeprefix(b"\x1b[200~").decode()
            # Repair controls append a separate user-instruction element.
            if text.startswith("<dauerbot_cli_repair"):
                wrapped = ET.fromstring("<fixture_controls>" + text + "</fixture_controls>")
                event = wrapped[0]
            else:
                event = ET.fromstring(text)
            if event.tag in {"dauerbot_cli_repair", "dauerbot_cli_repair_control"}:
                status("working")
                if native_repair_control(event, wrapped):
                    continue
                with (Path.cwd() / ".fixture-repairs").open("a") as repairs:
                    repairs.write(json.dumps({"generation": Path(sys.argv[2]).parent.name, "native": marker.read_text(), "payload": text}) + "\n")
                mode = Path.cwd() / ".fixture-repair-mode"
                if mode.exists() and mode.read_text() == "hold":
                    repair_held = event
                    status("idle")
                elif event.tag == "dauerbot_cli_repair":
                    repair_handoff(event)
                else:
                    status("idle")
                continue
            if event.tag == "dauerbot_cli_switch":
                status("working")
                mode = Path.cwd() / ".fixture-switch-mode"
                if mode.exists() and mode.read_text() == "hold":
                    (Path.cwd() / ".fixture-switch-held").write_text(event.attrib["request_id"])
                    switch_held = event
                    continue
                switch_handoff(event)
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
            delegation = event.find("management_delegation")
            context = delegation if delegation is not None else event
            body = context.findtext("manager_instructions") or context.findtext("feishu_message/body") or context.findtext("body") or ""
            if body in ("fixture-hold-unacknowledged", "fixture-hold-for-steer"):
                assert held is None
                held = event
                sys.stdout.write("\r\nMOCK_HOLDING_FOR_STEER\r\n")
                sys.stdout.flush()
                continue
            envelope = context.find("feishu_message")
            if envelope is not None:
                source = envelope.attrib["message_id"]
                to = envelope.find("source").attrib["chat_id"] or "oc_fixture"
                if source == "om_rich_post":
                    resources = envelope.findall("attachment")
                    assert body.count("fixture-post-caption") == 1, body
                    assert len(resources) == 1, resources
                    resource = resources[0].attrib
                    assert resource["resource_key"] == "img_fixture_post"
                    assert resource["trust"] == "user_input"
                    assert Path(resource["local_path"]).read_bytes() == b"downloaded fixture attachment"
                    (Path.cwd() / ".fixture-post-resource").write_text(json.dumps(resource))
                if "direct-to-user" in text:
                    to = envelope.find("sender").attrib["open_id"]
            else:
                source = event.attrib["source"] + ":" + event.attrib["event_id"]
                to = "oc_fixture"
            with (Path.cwd() / ".fixture-deliveries").open("a") as deliveries:
                deliveries.write(json.dumps({"source": source, "generation": Path(sys.argv[2]).parent.name, **({"manager_effect_id": delegation.attrib["effect_id"]} if delegation is not None else {})}) + "\n")
            reply_anchor = {"reply_to_message_id": source} if envelope is not None and to == envelope.find("source").attrib["chat_id"] else {}
            tool("send_md", {"to": to, "text": f"**Fixture result**\n\n{source}\n\n{text}", "request_id": "reply-" + (delegation.attrib["effect_id"] if delegation is not None else source), **reply_anchor})
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
