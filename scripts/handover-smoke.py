"""Isolated real-process ownership/SDK-handover regression; no real bot or Agent."""
import argparse
import hashlib
import concurrent.futures
import io
import sys
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("protocol_fixture", ROOT / "scripts/smoke.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--old-app", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    checks = []
    ack_codes=[]
    decode=fixture.proto_fields
    def inspect_ack(data):
        fields=decode(data)
        if 8 in fields:
            try:
                value=json.loads(fields[8][0])
                if "code" in value:ack_codes.append(value["code"])
            except (ValueError,UnicodeDecodeError):pass
        return fields
    fixture.proto_fields=inspect_ack
    with tempfile.TemporaryDirectory(prefix="db-ho-",dir="/tmp") as temporary:
        run = Path(temporary)
        data = run / "data"
        home = run / "home"
        home.mkdir()
        env = dict({k:v for k,v in os.environ.items() if not k.startswith(("HERDR_","HC_"))}, HOME=str(home), SHELL="/bin/sh", XDG_CONFIG_HOME=str(run / "config"), XDG_RUNTIME_DIR=str(run / "runtime"))
        blocked=threading.Event()
        release=threading.Event()
        class HandoverHandler(fixture.Handler):
            def do_POST(self):
                if self.path.startswith("/open-apis/im/v1/messages"):
                    data=self.rfile.read(int(self.headers.get("Content-Length",0)))
                    if b"gate-block" in data:
                        blocked.set()
                        assert release.wait(20),"fixture block was not released"
                    if self.headers.get("Authorization")=="Bearer fixture-token-cli_third":
                        self.headers.replace_header("Authorization","Bearer fixture-token-cli_second")
                    original=self.rfile
                    self.rfile=io.BytesIO(data)
                    try:return super().do_POST()
                    finally:self.rfile=original
                return super().do_POST()
        fixture.FIXTURE.server = fixture.ThreadingHTTPServer(("127.0.0.1", 0), HandoverHandler)
        threading.Thread(target=fixture.FIXTURE.server.serve_forever, daemon=True).start()
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        sock.close()
        log = (run / "service.log").open("w")
        process = subprocess.Popen([str(binary), "--data-dir", str(data), "serve", "--port", str(port), "--feishu-base", f"http://127.0.0.1:{fixture.FIXTURE.server.server_port}"], env=env, stdout=log, stderr=log)
        candidates = []
        try:
            config = fixture.wait_for(lambda: json.loads((data / "config.json").read_text()) if (data / "config.json").exists() else None, "configuration missing")
            def api(path, body=None, method=None, timeout=110):
                req = urllib.request.Request(f"http://127.0.0.1:{port}/api{path}", data=None if body is None else json.dumps(body).encode(), method=method or ("POST" if body is not None else "GET"), headers={"Content-Type":"application/json", "Authorization":"Bearer " + config["token"],"Connection":"close"})
                try:
                    with urllib.request.urlopen(req, timeout=timeout) as response:
                        return json.load(response)
                except urllib.error.HTTPError as error:
                    error.fixture_body=error.read().decode()
                    print(path,error.fixture_body)
                    raise
            state = fixture.wait_for(lambda: api("/state"), "initial owner not ready")
            assert state["service_activation"]["capability"] == 1
            (run / "workspace").mkdir()
            (run / "new-workspace").mkdir()
            project = api("/projects", {"name":"Handover fixture", "path":str(run / "workspace"), "agent":"custom", "extra_args":["/bin/true"]})
            api(f"/projects/{project['id']}/pause", {"paused":True})
            api(f"/projects/{project['id']}/bot", {"app_id":"cli_fixture", "app_secret":"fixture"}, "PUT")
            fixture.wait_for(lambda: len(fixture.FIXTURE.connections) == 1, "initial SDK handshake missing")
            old_connection = fixture.FIXTURE.connections[0]
            dbpath = data / "bridge.sqlite3"
            def count(source):
                with sqlite3.connect(dbpath) as c:
                    return c.execute("SELECT count(*) FROM messages WHERE source_id=?", [source]).fetchone()[0]
            def emit_to(conn, source):
                conn.sendall(fixture.ws_frame(fixture.event_frame(fixture.message(source, "handover fixture"))))
                fixture.wait_for(lambda: count(source) == 1, "event not journaled")
            def prepare():
                id = uuid.uuid4().hex
                r = api("/service/activation/prepare", {"activation_id":id,"candidate_executable":str(binary)})
                assert r["phase"] == "prepared" and r["transport_ready"] and not r["owner_committed"], r
                candidates.append(r["candidate_pid"])
                return id, r
            id, ready = prepare()
            fixture.wait_for(lambda: len(fixture.FIXTURE.connections) == 2, "candidate SDK handshake missing")
            candidate_connection = fixture.FIXTURE.connections[-1]
            before = fixture.FIXTURE.acks
            emit_to(candidate_connection, "warm-relay")
            fixture.wait_for(lambda: fixture.FIXTURE.acks > before, "candidate acknowledged before journal commit or failed ACK")
            assert count("warm-relay") == 1
            assert api("/state")["service_activation"]["owner_pid"] == process.pid
            checks.append("candidate WebSocket handshake and callback relay commit through old sole owner")
            duplicate = api("/service/activation/prepare", {"activation_id":id,"candidate_executable":str(binary)})
            assert duplicate["candidate_pid"] == ready["candidate_pid"]
            aborted = api("/service/activation/abort", {"activation_id":id})
            assert aborted["phase"] == "aborted"
            fixture.wait_for(lambda: len(fixture.FIXTURE.connections) == 1, "aborted candidate ingress still alive")
            emit_to(old_connection, "after-abort")
            checks.append("prepare retry reuses candidate; precommit abort preserves original listener/transport/journal")
            id, ready = prepare()
            api("/projects", {"name":"New identity during preparation", "path":str(run / "new-workspace"), "agent":"custom","extra_args":["/bin/true"]})
            failed = api("/service/activation/commit", {"activation_id":id})
            assert failed["phase"] == "failed" and not failed["owner_committed"], failed
            assert api("/state")["service_activation"]["owner_pid"] == process.pid
            api("/service/activation/abort", {"activation_id":id})
            checks.append("configuration snapshot fence prevents stale project/bot preparation from committing")
            id, ready = prepare()
            os.kill(ready["candidate_pid"], signal.SIGKILL)
            time.sleep(.1)
            try:
                api("/service/activation/commit", {"activation_id":id})
                raise AssertionError("dead standby committed")
            except urllib.error.HTTPError:
                pass
            abandoned_id=id
            id,replacement=prepare()
            assert id!=abandoned_id and replacement["candidate_pid"]!=ready["candidate_pid"]
            api("/service/activation/abort", {"activation_id":id})
            assert process.poll() is None
            checks.append("candidate death before commit leaves old owner serving; a different transaction ID reclaims the proven-dead standby slot")
            native=[]
            def legacy_rpc(method,params):
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as stream:
                    stream.settimeout(10)
                    stream.connect(str(run / "config/herdr/sessions" / config["herdr_session"] / "herdr.sock"))
                    stream.sendall((json.dumps({"id":uuid.uuid4().hex,"method":method,"params":params})+"\n").encode())
                    value=json.loads(stream.makefile().readline())
                    assert "error" not in value,value
                    return value["result"]
            if args.old_app:
                old=args.old_app.resolve()
                subprocess.run(["/usr/bin/codesign","--verify","--deep","--strict","-R",'=anchor apple generic and certificate leaf[subject.OU] = "366ADQ28F6" and identifier "dev.herdr.feishu-bridge"',str(old)],check=True,capture_output=True)
                for n,bot in [(1,"cli_second"),(2,"cli_third")]:
                    workspace=run / f"native{n}"
                    workspace.mkdir()
                    p=api("/projects",{"name":f"Published native MCP{n}","path":str(workspace),"agent":"custom","extra_args":[sys.executable,str(ROOT/"scripts/upgrade_agent.py"),str(old/"Contents/MacOS/hfb"),"{credentials}","{native_session}","--native-hook-fixture"]})
                    api(f"/projects/{p['id']}/bot",{"app_id":bot,"app_secret":"fixture"},"PUT")
                    api(f"/projects/{p['id']}/start",{})
                    fixture.wait_for(lambda:api(f"/projects/{p['id']}")["project"]["ready"],"native published MCP not ready")
                    fixture.wait_for(lambda:(workspace/"launches.jsonl").exists(),"native published MCP startup failed")
                    native.append((api(f"/projects/{p['id']}")["project"],workspace))
            id, ready = prepare()
            # Deliberately verify a clean handover does not run crash recovery
            # against live in-flight metadata merely by preparing/opening a copy.
            with sqlite3.connect(dbpath) as c:
                c.execute("INSERT INTO messages(id,project_id,direction,source_id,content,status,created_at,updated_at) VALUES(?,?,'out','inflight-fixture','fixture','sending',0,0)", [uuid.uuid4().hex, project['id']])
            candidate_connection = fixture.FIXTURE.connections[-1]
            if native:
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                    payload=json.dumps({"to":"oc_fixture","content":"gate-block","request_id":"gate-block"}).encode()
                    abandoned=socket.create_connection(("127.0.0.1",port),timeout=3)
                    abandoned.sendall((f"POST /api/projects/{project['id']}/outbox HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {config['token']}\r\nContent-Type: application/json\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n\r\n").encode()+payload)
                    assert blocked.wait(10)
                    abandoned.close()
                    committing=pool.submit(api,"/service/activation/commit",{"activation_id":id})
                    fixture.wait_for(lambda:api("/state")["service_activation"]["admission_closed"],"admission did not close")
                    for (p,workspace),body in zip(native,["contract-exercise-during","contract-restart"]):
                        envelope=f'<dauerbot_event event_id="{uuid.uuid4().hex}" run_id="fixture"><body>{body}</body></dauerbot_event>'
                        legacy_rpc("pane.send_input",{"pane_id":p["execution"]["pane_id"],"text":envelope,"keys":["Enter"]})
                    retry_event=fixture.message("during-closed-admission","deferred ingress")
                    warmed=next(conn for conn in reversed(fixture.FIXTURE.connections) if fixture.FIXTURE.connection_bots[conn]=="cli_fixture")
                    previous_errors=ack_codes.count(500)
                    warmed.sendall(fixture.ws_frame(fixture.event_frame(retry_event)))
                    fixture.wait_for(lambda:ack_codes.count(500)>previous_errors,"closed owner callback returned success instead of withholding provider ACK",timeout=5)
                    assert count("during-closed-admission")==0
                    assert not (native[0][1]/"contract-results.jsonl").exists()
                    assert len((native[1][1]/"launches.jsonl").read_text().splitlines())==1
                    release.set()
                    committed=committing.result(timeout=45)
                    with sqlite3.connect(dbpath) as c:
                        assert c.execute("SELECT status FROM messages WHERE content='gate-block'").fetchone()[0]=="sent"
                fixture.wait_for(lambda:(native[0][1]/"contract-results.jsonl").exists(),"old running stdio tool failed through drain/proxy")
                fixture.wait_for(lambda:len((native[1][1]/"launches.jsonl").read_text().splitlines())==2,"old MCP startup did not complete through drain/proxy")
                for p,workspace in native:
                    after=api(f"/projects/{p['id']}")["project"]
                    assert (after["generation"],after["terminal"],after["native_session"])==(p["generation"],p["terminal"],p["native_session"])
                warmed.sendall(fixture.ws_frame(fixture.event_frame(retry_event)))
                fixture.wait_for(lambda:count("during-closed-admission")==1,"provider retry did not commit exactly once")
                checks.append("closed-admission callback withholds ACK200 within3s; provider retry/late relay commits exactly one source receipt")
                checks.append("cancelled HTTP waiter retains admitted external effect through final receipt; actual signed published0.4.7 running stdio tool and fresh original startup wait through closed admission and resume without native identity change")
            else:
                committed = api("/service/activation/commit", {"activation_id":id})
            assert committed["owner_committed"] and committed["phase"] == "serving", committed
            new_state = api("/state")
            assert new_state["service_activation"]["owner_pid"] == ready["candidate_pid"]
            assert new_state["api_protocol"] == state["api_protocol"]
            with sqlite3.connect(dbpath) as c:
                assert c.execute("SELECT status FROM messages WHERE source_id='inflight-fixture'").fetchone()[0] == "uncertain"
            emit_to(candidate_connection, "after-commit")
            emit_to(old_connection, "old-relay-after-commit")
            assert count("warm-relay") == count("after-commit") == count("old-relay-after-commit") == 1
            process.wait(timeout=10)
            assert process.returncode == 0
            checks.append("same listener/port/token transfers to expected PID; abandoned effect metadata remains uncertain; old ingress relays before drain exit")
            status = api(f"/service/activation/status?activation_id={id}")
            assert status["owner_committed"] and status["phase"] == "serving"
            before=fixture.FIXTURE.connects
            fixture.FIXTURE.disconnect("cli_fixture")
            fixture.wait_for(lambda:fixture.FIXTURE.connects>before,"adopted warmed socket was not supervised/reconnected")
            fresh=next(conn for conn in fixture.FIXTURE.connections if fixture.FIXTURE.connection_bots[conn]=="cli_fixture")
            emit_to(fresh,"after-warm-reconnect")
            checks.append("adopted warmed transport reconnects after network loss and journals subsequent event")
            id,ready=prepare()
            files=data/"files"
            saved=data/".fixture-files-before-failure"
            files.rename(saved)
            files.write_text("transient startup failure fixture")
            killed=[]
            def crash_committed():
                deadline=time.monotonic()+20
                while time.monotonic()<deadline:
                    receipt=json.loads((data/"service-activation.json").read_text())
                    if receipt.get("activation_id")==id and receipt.get("owner_committed"):
                        try:os.kill(ready["candidate_pid"],signal.SIGKILL)
                        except ProcessLookupError:pass  # The private obstacle already ended startup.
                        files.unlink()
                        saved.rename(files)
                        killed.append(True)
                        return
                    time.sleep(.001)
                raise AssertionError("owner boundary not reached")
            crash=threading.Thread(target=crash_committed)
            crash.start()
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                recovery=pool.submit(api,"/service/activation/commit",{"activation_id":id})
                fixture.wait_for(lambda:killed,"postcommit death not injected")
                crash.join(timeout=5)
                recovered=recovery.result(timeout=60)
            assert recovered["phase"]=="serving" and recovered["owner_committed"],recovered
            assert recovered["candidate_pid"]!=ready["candidate_pid"]
            candidates.append(recovered["candidate_pid"])
            assert api("/state")["service_activation"]["owner_pid"]==recovered["candidate_pid"]
            with sqlite3.connect(dbpath) as c:
                assert c.execute("SELECT status FROM messages WHERE source_id='inflight-fixture'").fetchone()[0]=="uncertain"
            if native:
                for p,workspace in native:
                    after=api(f"/projects/{p['id']}")["project"]
                    assert (after["generation"],after["terminal"],after["native_session"])==(p["generation"],p["terminal"],p["native_session"])
                assert sum("contract-text" in m.get("content","") for m in fixture.FIXTURE.messages)==1
            checks.append("candidate death after durable commit automatically starts fresh new-owner epoch from latest journal, marks interrupted effects uncertain, preserves native sessions without duplicate sends")
            api("/service/shutdown", {})
            fixture.wait_for(lambda: not fixture.FIXTURE.connections, "new transports did not stop")
            checks.append("committed receipt survives original HTTP owner exit and shutdown drains new transport")
            evidence = {"binary":str(binary), "binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(), "checks":checks, "count":len(checks)}
            output = ROOT / "test-results/handover-smoke.json"
            output.parent.mkdir(exist_ok=True)
            output.write_text(json.dumps(evidence, indent=2) + "\n")
            print(json.dumps(evidence, indent=2))
        except BaseException:
            log.flush()
            print((run / "service.log").read_text()[-18000:])
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            for pid in candidates:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if (run / "config/herdr/sessions" / config["herdr_session"] / "herdr.sock").exists():
                subprocess.run([config["herdr"],"--session",config["herdr_session"],"server","stop"],env=env,capture_output=True,timeout=15)
            fixture.FIXTURE.server.shutdown()
            log.close()

if __name__ == "__main__":
    main()
