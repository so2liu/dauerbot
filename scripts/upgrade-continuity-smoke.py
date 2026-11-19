"""Private real-process manager/worker/MCP handover regression.

Uses scripted loopback providers and a native-protocol worker fixture, not an
account-backed model or Codex conversation. No installed app or service is used.
"""
import argparse
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import sys
import threading
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("manager_fixture", ROOT / "scripts/manager-smoke.py")
manager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manager)
fixture = manager.fixture


class ProviderRelease(threading.Event):
    def wait(self, timeout=None):
        # The shared manager fixture normally caps a held request at 20 seconds.
        # This regression optionally crosses standby's 45-second idle timeout.
        return super().wait(100)


class Harness(manager.Harness):
    def api(self, path, data=None, method=None, token=None):
        if path != "/service/activation/commit":
            return super().api(path, data, method, token)
        request = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/api{path}",
            data=json.dumps(data).encode(),
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + self.config["token"]},
        )
        with urllib.request.urlopen(request, timeout=110) as response:
            return json.load(response)

    def create(self):
        self.project = self.api("/projects", {
            "name": "Upgrade continuity fixture", "path": str(self.workspace),
            "agent": "custom", "extra_args": [sys.executable,
                str(ROOT / "scripts/upgrade_agent.py"), "{bridge_exe}",
                "{credentials}", "{native_session}", "--native-hook-fixture"],
        })
        self.key = self.project["id"]
        self.route = f"/projects/{self.key}"
        self.api(self.route + "/bot", {"app_id": "cli_fixture", "app_secret": "fixture-secret"}, "PUT")
        self.api(self.route + "/start", {})
        fixture.wait_for(lambda: self.api(self.route)["project"]["ready"], "worker MCP not ready", timeout=40)
        fixture.wait_for(lambda: self.launches(), "worker launch evidence missing")

    def launches(self):
        path = self.workspace / "launches.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def rows(self, sql, parameters=()):
        with sqlite3.connect((self.run / "bridge.sqlite3").as_uri() + "?mode=ro", uri=True) as db:
            db.execute("PRAGMA query_only=ON")
            return db.execute(sql, parameters).fetchall()

    def pane_input(self, body):
        project = self.api(self.route)["project"]
        socket_path = Path(self.env["XDG_CONFIG_HOME"]) / "herdr/sessions" / self.session / "herdr.sock"
        event = f'<dauerbot_event event_id="{uuid.uuid4().hex}" run_id="fixture"><body>{body}</body></dauerbot_event>'
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(5)
            stream.connect(str(socket_path))
            stream.sendall((json.dumps({"id": uuid.uuid4().hex, "method": "pane.send_input", "params": {
                "pane_id": project["execution"]["pane_id"], "text": event, "keys": ["Enter"],
            }}) + "\n").encode())
            response = json.loads(stream.makefile().readline())
            assert "error" not in response, response

    def enable_manager(self):
        config = self.api(self.route + "/manager")
        config = self.api(self.route + "/manager", {
            "base_url": f"http://127.0.0.1:{self.model.server_port}/v1",
            "model": "fixture-manager", "api_key": "manager-fixture-secret",
            "expected_revision": config["revision"],
        }, "PUT")
        self.api(self.route + "/agent/mode", {
            "mode": "manager", "request_id": "upgrade-fixture-enable",
            "expected_revision": config["revision"],
        })

    def prepare(self):
        activation = uuid.uuid4().hex
        ready = self.api("/service/activation/prepare", {
            "activation_id": activation, "candidate_executable": str(self.binary),
        })
        assert ready["phase"] == "prepared" and not ready["owner_committed"], ready
        return activation, ready


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--hold-seconds", type=float, default=0,
                        help="Keep the provider blocked during quiesce; use 46 to cross standby's former 45-second idle timeout")
    args = parser.parse_args()
    assert 0 <= args.hold_seconds <= 80, "provider hold must fit the bounded activation deadline"
    manager.MODEL.release = ProviderRelease()
    h = Harness(args.binary)
    evidence = {"scope": "Isolated actual daemon/HERDR/stdio MCP; scripted provider and native-protocol worker, not real Codex model execution", "checks": h.checks}
    try:
        h.boot()
        h.create()
        before = h.api(h.route)["project"]
        launch = h.launches()[0]
        h.pane_input("hold-before-upgrade")
        received = h.workspace / "received.jsonl"
        fixture.wait_for(lambda: received.exists() and "hold-before-upgrade" in received.read_text(), "worker did not begin held work")
        h.enable_manager()

        source = "om_upgrade_continuity"
        marker = "upgrade-checkpoint.txt"
        manager.MODEL.script(
            {"wait": True, "tool": "write_file", "id": "upgrade-first-call", "arguments": {"path": marker, "content": "accepted original response\n"}},
            {"tool": "send_md", "arguments": {"to": "oc_fixture", "reply_to_message_id": source, "text": "upgrade-continuity-complete"}},
            {"text": ""},
            trigger=source,
        )
        fixture.FIXTURE.emit(fixture.message(source, "Complete this manager turn across the prepared upgrade."))
        assert manager.MODEL.started.wait(10), "controlled provider did not start"
        run_before = h.rows("SELECT id,owner FROM manager_runs WHERE project_id=? AND status='working'", [h.key])
        assert len(run_before) == 1, run_before
        request_start = len(manager.MODEL.requests)
        original_request = manager.MODEL.requests[request_start - 1]
        activation, ready = h.prepare()
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            hold_started = time.monotonic()
            committing = pool.submit(h.api, "/service/activation/commit", {"activation_id": activation})
            fixture.wait_for(lambda: h.api("/state")["service_activation"].get("manager_quiescing"), "manager admission was not quiesced")
            state = h.api("/state")["service_activation"]
            assert state["admission_closed"] is False, "MCP/controls were blocked while waiting for the provider"
            assert not committing.done(), "commit interrupted the running manager instead of waiting"
            assert h.daemon.poll() is None
            assert len(manager.MODEL.requests) == request_start, "blocked provider request was already replayed"
            # The existing worker actively calls all three tools while its
            # manager is quiescing. No second native process is launched.
            h.pane_input("contract-exercise-during-manager-quiesce")
            contract_results = h.workspace / "contract-results.jsonl"
            fixture.wait_for(lambda: contract_results.exists(), "running worker/MCP could not work during manager quiesce")
            assert h.rows("SELECT id,owner FROM manager_runs WHERE project_id=? AND status='working'", [h.key]) == run_before
            remaining_hold = args.hold_seconds - (time.monotonic() - hold_started)
            if remaining_hold > 0:
                time.sleep(remaining_hold)
            assert not committing.done(), "activation stopped waiting before the controlled response was released"
            manager.MODEL.release.set()
            result = committing.result(timeout=30)
        assert result["owner_committed"] and result["phase"] == "serving", result
        fixture.wait_for(lambda: any("upgrade-continuity-complete" in m.get("content", "") for m in fixture.FIXTURE.messages), "manager did not resume after handover")
        fixture.wait_for(lambda: not h.rows("SELECT 1 FROM manager_runs WHERE project_id=? AND status='working'", [h.key]), "resumed manager turn did not finish")
        after = h.api(h.route)["project"]
        for field in ["generation", "terminal", "native_session", "execution"]:
            assert after[field] == before[field], (field, before[field], after[field])
        assert h.launches() == [launch], "native worker or MCP was restarted"
        for pid in [launch["pid"], launch["mcp_pid"]]:
            os.kill(pid, 0)
        assert (h.workspace / marker).read_text() == "accepted original response\n"
        effects = h.rows(
            "SELECT e.run_id,e.status FROM manager_effects e WHERE e.run_id=? AND e.name='write_file'", [run_before[0][0]])
        assert effects == [(run_before[0][0], "complete")], effects
        run_after = h.rows("SELECT id,owner,status FROM manager_runs WHERE id=?", [run_before[0][0]])
        assert len(run_after) == 1 and run_after[0][2] == "complete", run_after
        assert run_after[0][1] != run_before[0][1], "new owner did not adopt the original run"
        sent = [m for m in fixture.FIXTURE.messages if "upgrade-continuity-complete" in m.get("content", "")]
        assert len(sent) == 1, sent
        assert not h.api(h.route + "/manager")["error"]
        original_request_count = sum(request == original_request for request in manager.MODEL.requests)
        assert original_request_count == 1, "original blocked provider request was replayed after cutover"
        h.checks.append("Blocked provider response completes once; all tool results checkpoint before original run is adopted by the new owner")
        h.checks.append("Existing native worker and stdio MCP call all three tools during quiesce and retain exact PIDs, launch count, generation and native identity")
        h.checks.append("Exactly one committed write and one final topic reply; no residual manager failure")
        h.daemon.wait(timeout=10)

        # A candidate whose prepared configuration became stale must refuse
        # ownership, reopen admission and let the old manager continue.
        activation, _ = h.prepare()
        new_workspace = h.run / "new-identity"
        new_workspace.mkdir()
        h.api("/projects", {"name": "New identity after preparation", "path": str(new_workspace),
            "agent": "custom", "extra_args": ["/bin/true"]})
        failed = h.api("/service/activation/commit", {"activation_id": activation})
        assert failed["phase"] == "failed" and not failed["owner_committed"], failed
        state = h.api("/state")["service_activation"]
        assert not state["admission_closed"] and not state["manager_quiescing"], state
        assert state["owner_pid"] == ready["candidate_pid"], state
        h.api("/service/activation/abort", {"activation_id": activation})
        manager.MODEL.script({"tool": "write_file", "arguments": {"path": "after-refusal.txt", "content": "old owner remains usable\n"}}, {"text": ""}, trigger="om_upgrade_refusal")
        fixture.FIXTURE.emit(fixture.message("om_upgrade_refusal", "Continue after the refused candidate."))
        fixture.wait_for(lambda: (h.workspace / "after-refusal.txt").exists(), "old manager did not continue after refused activation")
        h.checks.append("Changed prepared configuration refuses cutover and reopens both admission gates; old manager continues real tool work")
        evidence.update(passed=True, manager_run=run_before[0][0], native_pid=launch["pid"], mcp_pid=launch["mcp_pid"], checks_count=len(h.checks), provider_hold_seconds=args.hold_seconds,
                        provider_request_count=len(manager.MODEL.requests), original_provider_request_count=original_request_count)
    except Exception:
        evidence["passed"] = False
        raise
    finally:
        evidence["run_dir"] = str(h.run)
        output = args.evidence_dir or ROOT / "test-results"
        output.mkdir(parents=True, exist_ok=True)
        (output / "upgrade-continuity-smoke.json").write_text(json.dumps(evidence, indent=2) + "\n")
        # Only the fixture owner is shut down; its replacement uses the same
        # private endpoint. Harness.close then removes its private HERDR server.
        try:
            h.api("/service/shutdown", {})
        except Exception:
            pass
        h.close()
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
