"""Private model/card integration through actual SDK WebSockets, HERDR and MCP.

The Codex executable is a deterministic CLI shim. Its native ID is seeded only
in this private store: these checks do not establish live-model/account or
vendor-native resume acceptance. No real configuration or bot is touched.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import sqlite3
import subprocess
import sysconfig
import sys
import tempfile
import threading
import urllib.request
import uuid
from http.server import ThreadingHTTPServer

import smoke

ROOT = Path(__file__).resolve().parent.parent
APP = "cli_model_fixture"


def callback(card, event_id, chosen="m1", operator="ou_fixture"):
    return {"schema": "2.0", "header": {
        "event_type": "card.action.trigger", "event_id": event_id, "app_id": APP,
    }, "event": {
        "operator": {"open_id": operator}, "host": "im_message", "token": "fixture-callback-token",
        "context": {"open_message_id": card["message_id"], "open_chat_id": "oc_fixture"},
        "action": {"tag": "button", "name": card["button"], "form_value": {"catalog_model": chosen}},
    }}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    assert binary.is_file(), binary
    checks = []
    daemon = None
    cfg = None
    project_id = None
    session = "model-" + uuid.uuid4().hex[:8]
    evidence = {"scope": "Private deterministic Codex CLI shim; real SDK WebSocket/card callback, HERDR and MCP; not live model acceptance"}
    with tempfile.TemporaryDirectory(prefix="db-model-", dir="/tmp") as folder:
        base = Path(folder)
        home = base / "home"
        codex = home / ".codex"
        codex.mkdir(parents=True)
        (codex / "config.toml").write_text('model = "fixture-model-one"\nmodel_provider = "fixture-provider"\n')
        (codex / "models_cache.json").write_text(json.dumps({"models": [
            {"slug": "fixture-model-one", "display_name": "Fixture One", "visibility": "list"},
            {"slug": "fixture-model-two", "display_name": "Fixture Two", "visibility": "list"},
            {"slug": "fixture-hidden", "display_name": "Hidden", "visibility": "hide"},
        ]}))
        workspace = base / "work"
        workspace.mkdir()
        data = base / "data"
        data.mkdir()
        bindir = home / "bin"
        bindir.mkdir()
        (home / ".profile").write_text('export PATH="' + str(bindir) + ':$PATH"\n')
        shim = bindir / "codex-fixture.py"
        shim.write_text(f'''import json,os,runpy,sys
if '--help' in sys.argv or '--version' in sys.argv:
 print('codex-cli 0.167.2');sys.exit(0)
a=sys.argv[1:]
entry=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.command=')))
config=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.args=')))
model='fixture-model-one';provider='fixture-provider'
for i,value in enumerate(a):
 if value in ('--model','-m'):model=a[i+1]
 elif value.startswith('--model='):model=value.split('=',1)[1]
 elif value.startswith('model_provider='):provider=json.loads(value.split('=',1)[1])
native=a[a.index('resume')+1] if 'resume' in a else ''
metadata={{'argv':a,'model':model,'provider':provider,'native_resume':native,'pid':os.getpid()}}
sys.argv=[{str(ROOT / 'scripts/mock_agent.py')!r},entry,config[-1],native,a[-1],'codex',json.dumps(metadata)]
runpy.run_path(sys.argv[0],run_name='__main__')
''')
        # Framework Python on macOS replaces an exec -a argv0 with Python.
        # Embed the deterministic fixture in a real codex-named process so both
        # HERDR and Dauerbot's strict process whitelist run unchanged.
        launcher = bindir / "launcher.c"
        launcher.write_text("#include <Python.h>\n#include <stdlib.h>\nint main(int argc,char **argv){\n"
            "char **args=calloc((size_t)argc+2,sizeof(char*)); if(!args)return 70;\n"
            "args[0]=argv[0]; args[1]=" + json.dumps(str(shim)) + ";\n"
            "for(int i=1;i<argc;i++)args[i+1]=argv[i];\n"
            "int status=Py_BytesMain(argc+1,args); free(args); return status; }\n")
        framework = sysconfig.get_config_var("PYTHONFRAMEWORK")
        link = ["-F", sysconfig.get_config_var("PYTHONFRAMEWORKPREFIX"), "-framework", framework] if framework else [
            "-L", sysconfig.get_config_var("LIBDIR"), "-lpython" + sysconfig.get_config_var("LDVERSION"),
            "-Wl,-rpath," + sysconfig.get_config_var("LIBDIR"),
        ] + shlex.split(sysconfig.get_config_var("LIBS") or "") + shlex.split(sysconfig.get_config_var("SYSLIBS") or "")
        subprocess.run(["cc", "-I", sysconfig.get_path("include"), str(launcher), *link, "-o", str(bindir / "codex")],
                       check=True, capture_output=True, text=True, timeout=30)
        runtime = bindir / "herdr"
        shutil.copyfile(shutil.which("herdr"), runtime)
        runtime.chmod(0o700)
        env = {k: v for k, v in os.environ.items() if not k.startswith(("HERDR_", "HC_", "SSH_")) and k not in {
            "CODEX_HOME", "CLAUDE_CONFIG_DIR", "PI_CODING_AGENT_DIR", "GEMINI_CLI_HOME"}}
        env.update(HOME=str(home), CODEX_HOME=str(codex), XDG_CONFIG_HOME=str(base / "xdg"),
                   XDG_STATE_HOME=str(base / "state"), SHELL="/bin/sh", PATH=str(bindir) + ":" + os.environ["PATH"])
        smoke.FIXTURE = smoke.Fixture()
        smoke.FIXTURE.server = ThreadingHTTPServer(("127.0.0.1", 0), smoke.Handler)
        threading.Thread(target=smoke.FIXTURE.server.serve_forever, daemon=True).start()
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        log = (data / "daemon.log").open("w")

        def sql(query, params=()):
            with sqlite3.connect(f"file:{data / 'bridge.sqlite3'}?mode=ro", uri=True, timeout=10) as db:
                return db.execute(query, params).fetchall()

        def api(path, payload=None, method=None):
            request = urllib.request.Request(
                f"http://127.0.0.1:{port}/api" + path,
                data=None if payload is None else json.dumps(payload).encode(),
                headers={"Authorization": "Bearer " + cfg["token"], "Content-Type": "application/json"},
                method=method or ("POST" if payload is not None else "GET"))
            with urllib.request.urlopen(request, timeout=20) as response:
                return json.load(response)

        def project():
            return next(p for p in api("/state")["projects"] if p["id"] == project_id)

        def launches():
            path = workspace / ".fixture-launches"
            return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

        def card_for(source):
            rows = sql("SELECT c.id,c.token,c.options,m.id,m.status,m.attachment FROM model_control_commands c JOIN messages m ON m.id=c.outbox_id WHERE c.project_id=? AND c.source_id=?", (project_id, source))
            if not rows or rows[0][4] != "sent":
                return None
            command, token, options, output, _, attachment = rows[0]
            attachment = json.loads(attachment)
            sent = next((m for m in smoke.FIXTURE.messages if m["uuid"] == output), None)
            if sent is None:
                return None
            assert sent["msg_type"] == "interactive" and sent["fixture_reply_to"] == source
            card = json.loads(sent["content"])
            form = next(e for e in card["body"]["elements"] if e["tag"] == "form")
            button = next(e["name"] for e in form["elements"] if e["tag"] == "button")
            assert button == "apply_" + token
            select = next(e for e in form["elements"] if e["name"] == "catalog_model")
            assert card["schema"] == "2.0" and attachment["sent_root_id"] == source
            return {"command": command, "message_id": attachment["sent_message_id"], "button": button,
                    "options": json.loads(options), "card": card, "select": select}

        def emit_action(event, frame=None, expected="info"):
            identity = frame or event["header"]["event_id"]
            smoke.FIXTURE.emit_callback(event, APP, identity)
            result = smoke.wait_for(lambda: next((v for v in smoke.FIXTURE.callback_responses if v["frame_id"] == identity), None), "no native SDK callback ACK: " + identity)
            assert result["code"] == 200 and result["response"]["toast"]["type"] == expected, result
            return result["response"]["toast"].get("content", "")

        def held(source):
            state = api(f"/projects/{project_id}/work")
            events = [e for e in state["events"] if e["source_id"] == source]
            if not events:
                return None
            assert len(events) == 1 and events[0]["status"] == "queued", state
            assert not any(r["event_id"] == events[0]["id"] for r in state["runs"]), state
            return state

        try:
            daemon = subprocess.Popen([str(binary), "--data-dir", str(data), "serve", "--port", str(port),
                "--herdr", str(runtime), "--herdr-session", session, "--feishu-base",
                f"http://127.0.0.1:{smoke.FIXTURE.server.server_port}"], env=env, stdout=log, stderr=log)
            cfg = smoke.wait_for(lambda: json.loads((data / "config.json").read_text()) if (data / "config.json").exists() else None, "no private daemon config")
            smoke.wait_for(lambda: api("/state"), "private daemon unavailable")
            created = api("/projects", {"name": "Native model/card fixture", "path": str(workspace), "agent": "codex",
                "extra_args": ["--model", "fixture-model-one", "--add-dir", str(base / "unrelated path")]})
            project_id = created["id"]
            api(f"/projects/{project_id}/bot", {"app_id": APP, "app_secret": "fixture-secret"}, "PUT")
            smoke.wait_for(lambda: APP in smoke.FIXTURE.connection_bots.values(), "no isolated SDK websocket")
            api(f"/projects/{project_id}/start", {})
            smoke.wait_for(lambda: project()["ready"] and project()["execution"].get("mcp_registered") and launches(), "native shim/MCP not ready")
            native = (workspace / ".fixture-session-codex").read_text()
            original = project()
            with sqlite3.connect(data / "bridge.sqlite3", timeout=10) as db:
                db.execute("UPDATE projects SET native_session=?,execution=json_set(execution,'$.native_confirmed',json('true')) WHERE id=? AND generation=?", (native, project_id, original["generation"]))
                db.execute("UPDATE work_executions SET native_session=? WHERE project_id=? AND generation=?", (native, project_id, original["generation"]))
            catalog = api(f"/projects/{project_id}/models")
            assert "fixture-model-two" in json.dumps(catalog) and "fixture-hidden" not in json.dumps(catalog), catalog
            for source in ["om_model_picker", "om_model_stale_picker"]:
                smoke.FIXTURE.emit(smoke.message(source, "/models"), APP)
            card = smoke.wait_for(lambda: card_for("om_model_picker"), "no topic model picker card")
            stale = smoke.wait_for(lambda: card_for("om_model_stale_picker"), "no second model picker card")
            selected = next(i for i, option in enumerate(card["options"]) if option["model"] == "fixture-model-two")
            assert not sql("SELECT id FROM work_events WHERE source_id IN ('om_model_picker','om_model_stale_picker')")
            checks.append("real SDK /models ingress produces schema2 interactive cards in original topics; native cache discovery hides disabled entries; controls never reach Agent work queue")

            wrong = callback(card, "evt_model_wrong_owner", f"m{selected}", "ou_intruder")
            assert "requesting user" in emit_action(wrong, expected="error")
            unknown = callback(card, "evt_model_unknown", "m99999")
            assert "Unknown model" in emit_action(unknown, expected="error")
            assert sql("SELECT count(*) FROM model_control_actions")[0][0] == 0
            assert sql("SELECT count(*) FROM cli_switch_requests")[0][0] == 0
            checks.append("native SDK callback ACK carries error toast for wrong requester and unknown option; rejected actions create no transition")

            (workspace / ".fixture-switch-mode").write_text("hold")
            accepted = callback(card, "evt_model_accept", f"m{selected}")
            emit_action(accepted)
            smoke.wait_for(lambda: (workspace / ".fixture-switch-held").exists(), "model acceptance did not request safe source handoff")
            transition = project()["cli_transition"]
            assert transition["id"] == card["command"] and transition["holds_input"]
            assert sql("SELECT count(*) FROM project_model_preferences WHERE project_id=?", (project_id,))[0][0] == 0
            emit_action(accepted, "evt_model_accept_transport_retry")
            pending = "om_model_held_input"
            smoke.FIXTURE.emit(smoke.message(pending, "ordinary input stays held during model transition"), APP)
            smoke.wait_for(lambda: held(pending), "ordinary input was not held for new model")
            assert not (workspace / ".fixture-deliveries").exists()
            (workspace / ".fixture-mcp-hold").write_text("hold target MCP registration")
            (workspace / ".fixture-switch-mode").unlink()
            (workspace / ".fixture-switch-release").write_text("settle old model without replaying user work")
            smoke.wait_for(lambda: len(launches()) == 2, "requested model did not launch on same native session", timeout=50)
            target = launches()[-1]
            assert target["resume"] == native == target["native"]
            assert target["launch"]["model"] == "fixture-model-two"
            assert target["launch"]["provider"] == "fixture-provider"
            assert 'model_provider="fixture-provider"' in target["launch"]["argv"]
            assert target["launch"]["argv"].count("--model") == 1
            assert "--add-dir" in target["launch"]["argv"] and str(base / "unrelated path") in target["launch"]["argv"]
            held(pending)
            assert project()["cli_transition"]["holds_input"] and not project()["execution"].get("mcp_registered")
            checks.append("accepted callback is durable/idempotent; source preference stays unchanged until safe handoff; requested model/provider argv resumes same native ID and preserves unrelated args while input waits for MCP")
            (workspace / ".fixture-mcp-hold").unlink()
            current = smoke.wait_for(lambda: project() if project()["cli_transition"]["phase"] == "ready" else None, "model target did not become ready", timeout=50)
            def delivered():
                path = workspace / ".fixture-deliveries"
                if not path.exists():
                    return None
                rows = [json.loads(line) for line in path.read_text().splitlines()]
                if not rows:
                    return None
                assert rows == [{"source": pending, "generation": current["generation"]}], rows
                work = api(f"/projects/{project_id}/work")
                event = next(e for e in work["events"] if e["source_id"] == pending)
                runs = [r for r in work["runs"] if r["event_id"] == event["id"]]
                assert len(runs) == 1 and runs[0]["generation"] == current["generation"], work
                return rows
            smoke.wait_for(delivered, "held input did not reach selected model once")
            assert current["generation"] != original["generation"] and current["native_session"] == native
            assert current["execution"]["model_selection"]["model"] == "fixture-model-two"
            assert not current["cli_transition"]["holds_input"]
            emit_action(accepted, "evt_model_accept_retry_after_ready")
            stale_action = callback(stale, "evt_model_stale_generation", "m0")
            assert "changed" in emit_action(stale_action, expected="error")
            assert len(launches()) == 2
            assert sql("SELECT count(*) FROM cli_switch_requests")[0][0] == 1
            assert sql("SELECT count(*) FROM model_control_actions")[0][0] == 1
            checks.append("exact request/generation/native+MCP readiness releases held input once; callback replay after generation change is deduped and unused stale card is rejected")

            smoke.FIXTURE.emit(smoke.message("om_model_default", "/models"), APP)
            default = smoke.wait_for(lambda: card_for("om_model_default"), "no refreshed picker for native default")
            emit_action(callback(default, "evt_model_default", "native"))
            reset = smoke.wait_for(lambda: project() if len(launches()) == 3 and project()["cli_transition"]["phase"] == "ready" else None, "native-default reset did not finish", timeout=50)
            reset_launch = launches()[-1]
            assert reset_launch["resume"] == native and reset_launch["native"] == native
            assert "--model" not in reset_launch["launch"]["argv"]
            assert not any(value.startswith("model_provider=") for value in reset_launch["launch"]["argv"])
            assert "--add-dir" in reset_launch["launch"]["argv"]
            assert sql("SELECT count(*) FROM project_model_preferences WHERE project_id=?", (project_id,))[0][0] == 0
            assert reset["execution"]["model_selection"] is None
            checks.append("inherit-native-settings form action removes only project model/provider flags, preserves unrelated args and same native session, and clears explicit preference without claiming the session's effective model")
            api(f"/projects/{project_id}/stop", {})
            evidence.update(checks=checks, count=len(checks), launches=launches(), callback_responses=smoke.FIXTURE.callback_responses)
        except Exception as error:
            evidence.update(checks=checks, error=str(error))
            try:
                evidence["state"] = api("/state")
                evidence["launches"] = launches()
                evidence["daemon_log"] = (data / "daemon.log").read_text()
                if project_id:
                    evidence["screen"] = api(f"/projects/{project_id}/screen")
                evidence["callback_responses"] = smoke.FIXTURE.callback_responses
                evidence["repair_prompts"] = sql("SELECT generation,status,error FROM cli_repair_prompts ORDER BY rowid")
                evidence["control_commands"] = sql("SELECT source_id,phase,error FROM model_control_commands ORDER BY rowid")
            except Exception as diagnostic_error:
                evidence["diagnostic_error"] = str(diagnostic_error)
            raise
        finally:
            if daemon is not None:
                daemon.terminate()
                daemon.wait(timeout=15)
            log.close()
            if cfg:
                try:
                    subprocess.run([cfg["herdr"], "--session", session, "server", "stop"], env=env, capture_output=True, timeout=10)
                except (OSError, subprocess.TimeoutExpired) as cleanup_error:
                    evidence["cleanup_error"] = str(cleanup_error)
            smoke.FIXTURE.server.shutdown()
            smoke.FIXTURE.server.server_close()
            result = ROOT / "test-results/model-selection-smoke.json"
            result.parent.mkdir(exist_ok=True)
            result.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"checks": checks, "count": len(checks), "scope": evidence["scope"]}))


if __name__ == "__main__":
    main()
