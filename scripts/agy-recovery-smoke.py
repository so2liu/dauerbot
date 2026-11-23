"""Exercise AGY login recovery with a private HERDR 0.9.5 server and AGY shim."""
import argparse
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import sqlite3
import subprocess
import sys
import sysconfig
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

import smoke

NATIVE = "agy-fixture-saved-conversation"
REQUEST = "agy-fixture-exhausted-request"


def build_shim(bindir, base):
    shim = bindir / "fixture.py"
    shim.write_text(f'''import json,os,select,socket,sys,time,tty
from pathlib import Path
base=Path({str(base)!r})
args=sys.argv[1:]
if '--version' in args or '--help' in args:
 print('agy fixture');sys.exit(0)
native=args[args.index('--conversation')+1] if '--conversation' in args else ''
credential_at_start=(base/'startup-credential').is_file()
with (base/'launches.jsonl').open('a') as log:
 log.write(json.dumps({{'pid':os.getpid(),'native':native,'credential_at_start':credential_at_start,'pane':os.environ['HERDR_PANE_ID']}})+'\\n')
tty.setraw(0)
def rpc(method,params):
 with socket.socket(socket.AF_UNIX) as conn:
  conn.settimeout(5)
  conn.connect(os.environ['HERDR_SOCKET_PATH'])
  params.update(pane_id=os.environ['HERDR_PANE_ID'],seq=time.time_ns())
  conn.sendall((json.dumps({{'id':'agy-fixture','method':method,'params':params}})+'\\n').encode())
  result=json.loads(conn.makefile().readline())
  assert 'error' not in result, result
if not credential_at_start:
 print('You are currently not signed in\\r\\nSelect login method',flush=True)
 while True:
  rpc('pane.report_agent',{{'source':'dauerbot-fixture','agent':'agy','state':'blocked'}})
  time.sleep(0.25)
assert native == {NATIVE!r},'fresh AGY discarded saved native conversation'
print('Authenticated fixture AGY\\r',flush=True)
rpc('pane.report_agent_session',{{'source':'herdr:antigravity_cli','agent':'agy','agent_session_id':native}})
rpc('pane.report_agent',{{'source':'dauerbot-fixture','agent':'agy','state':'idle'}})
while not (base/'release-registration').exists():
 time.sleep(0.1)
entry=json.loads((Path.home()/'.gemini/config/mcp_config.json').read_text())['mcpServers']['herdr_feishu']
cred_dir=Path(entry['args'][entry['args'].index('--data-dir')+1])/'sessions'
credentials=next(p for p in cred_dir.glob('*/*/credentials.json') if p.parent.name != (base/'old-generation').read_text())
import urllib.request
creds=json.loads(credentials.read_text())
headers={{'Authorization':'Bearer '+creds['token'],'x-hfb-generation':creds['generation'],'Content-Type':'application/json'}}
def post(operation,value):
 url=creds['url']+'/api/projects/'+creds['project']+'/'+operation
 req=urllib.request.Request(url,json.dumps(value).encode(),headers,method='POST')
 with urllib.request.urlopen(req,timeout=15) as response:
  return json.load(response)
registration=post('register',{{'generation':creds['generation'],'terminal':os.environ['HERDR_PANE_ID'],'mcp_contract':1}})
assert registration['api_protocol']==7 and registration['mcp_contract']==1, registration
(base/'registered').touch()
while True:
 if select.select([0],[],[],0.2)[0]:
  data=os.read(0,4096)
  if not data: break
  with (base/'delivered').open('ab') as log: log.write(data)
''')
    launcher = bindir / "launcher.c"
    launcher.write_text(
        "#include <Python.h>\n#include <stdlib.h>\n"
        "int main(int argc,char **argv){\n"
        "char **args=calloc((size_t)argc+2,sizeof(char*));if(!args)return 70;\n"
        f"args[0]=argv[0];args[1]={json.dumps(str(shim))};\n"
        "for(int i=1;i<argc;i++)args[i+1]=argv[i];\n"
        "int result=Py_BytesMain(argc+1,args);free(args);return result;}\n"
    )
    framework = sysconfig.get_config_var("PYTHONFRAMEWORK")
    if framework:
        link = ["-F", sysconfig.get_config_var("PYTHONFRAMEWORKPREFIX"), "-framework", framework]
    else:
        libdir = sysconfig.get_config_var("LIBDIR")
        link = ["-L", libdir, "-lpython" + sysconfig.get_config_var("LDVERSION"), "-Wl,-rpath," + libdir]
        link += shlex.split(sysconfig.get_config_var("LIBS") or "")
        link += shlex.split(sysconfig.get_config_var("SYSLIBS") or "")
    subprocess.run(
        ["cc", "-I", sysconfig.get_path("include"), str(launcher), *link, "-o", str(bindir / "agy")],
        check=True, capture_output=True, text=True, timeout=30,
    )


def run(binary, herdr, base):
    base.mkdir(mode=0o700)
    home, data, project_dir = (base / name for name in ("home", "data", "workspace"))
    bindir = home / "bin"
    bindir.mkdir(parents=True)
    (home / ".gemini/config").mkdir(parents=True)
    project_dir.mkdir()
    for name in ("tmp", "xdg", "state", "cache", "local-share"):
        (base / name).mkdir()
    (home / ".profile").write_text(f'export PATH="{bindir}:$PATH"\n')
    build_shim(bindir, base)
    # The daemon prepends the runtime bin after login-shell setup. Keep that
    # directory private so a machine-installed AGY cannot replace the shim.
    private_herdr = bindir / "herdr"
    shutil.copyfile(herdr, private_herdr)
    private_herdr.chmod(0o700)
    herdr = private_herdr
    env = {
        "HOME": str(home), "SHELL": "/bin/sh", "PATH": f"{bindir}:/usr/bin:/bin",
        "XDG_CONFIG_HOME": str(base / "xdg"), "XDG_STATE_HOME": str(base / "state"),
        "XDG_CACHE_HOME": str(base / "cache"), "XDG_DATA_HOME": str(base / "local-share"),
        "TMPDIR": str(base / "tmp"), "NO_PROXY": "127.0.0.1",
    }
    session = f"agy-{uuid.uuid4().hex[:8]}"
    smoke.FIXTURE = smoke.Fixture()
    fixture = smoke.FIXTURE
    fixture.server = ThreadingHTTPServer(("127.0.0.1", 0), smoke.Handler)
    threading.Thread(target=fixture.server.serve_forever, daemon=True).start()
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    log = (base / "daemon.log").open("w")
    daemon = subprocess.Popen(
        [str(binary), "--data-dir", str(data), "serve", "--port", str(port), "--herdr", str(herdr),
         "--herdr-session", session, "--feishu-base", f"http://127.0.0.1:{fixture.server.server_port}"],
        env=env, stdout=log, stderr=log,
    )

    def api(path, body=None, method=None):
        request = urllib.request.Request(
            f"http://127.0.0.1:{port}/api{path}", None if body is None else json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {cfg['token']}", "Content-Type": "application/json"},
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{path}: {error.read().decode()}") from error

    def state():
        return next(item for item in api("/state")["projects"] if item["id"] == project)

    def launches():
        journal = base / "launches.jsonl"
        return [json.loads(line) for line in journal.read_text().splitlines()] if journal.exists() else []

    def events():
        rows = api(f"/projects/{project}/work")["events"]
        return {row["source_id"]: row for row in rows}

    try:
        config_file = data / "config.json"
        cfg = smoke.wait_for(
            lambda: json.loads(config_file.read_text()) if config_file.exists() else None,
            "no private daemon config",
        )
        smoke.wait_for(lambda: api("/state"), "private daemon did not start")
        project = api("/projects", {"name": "AGY recovery", "path": str(project_dir), "agent": "agy"})["id"]
        app_id = "cli_agy_repair"
        api(f"/projects/{project}/bot", {"app_id": app_id, "app_secret": "fixture-only"}, method="PUT")
        smoke.wait_for(lambda: app_id in fixture.connection_bots.values(), "private bot failed to connect")
        api(f"/projects/{project}/start", {})
        smoke.wait_for(lambda: len(launches()) == 1 and state()["terminal"], "initial AGY pane not started")
        old = state()
        old_generation = old["generation"]
        start = launches()[0]
        assert not start["credential_at_start"] and not old["ready"], old

        # The incident's pre-repair journal is simulated in this private store.
        # The pane, process, screen, and recovery requests remain real.
        now = int(time.time() * 1000)
        with sqlite3.connect(data / "bridge.sqlite3", timeout=10) as db:
            db.execute("UPDATE projects SET native_session=? WHERE id=?", (NATIVE, project))
            db.execute(
                """INSERT INTO cli_switch_requests(id,project_id,target,phase,generation,created_at,
                   holds_input,phase_started_at,recovery_attempts,recovery_generation,source)
                   VALUES(?,?,'agy','recovery_exhausted',?,?,1,?,12,?,?)""",
                (REQUEST, project, old_generation, now - 600_000, now, old_generation,
                 json.dumps({"agent": "agy", "native_session": NATIVE, "generation": old_generation,
                             "runtime_target": {"agent": "agy", "native_session": NATIVE}})),
            )
        (base / "old-generation").write_text(old_generation)
        pending, uncertain = "om_fixture_pending", "om_fixture_uncertain"
        fixture.emit(smoke.message(pending, "Deliver this queued input once"), app_id)
        smoke.wait_for(lambda: pending in events(), "queued fixture input was not journaled")
        fixture.emit(smoke.message(uncertain, "Do not replay this uncertain input"), app_id)
        smoke.wait_for(lambda: uncertain in events(), "uncertain fixture input was not journaled")
        with sqlite3.connect(data / "bridge.sqlite3", timeout=10) as db:
            for table in ("messages", "work_events"):
                assert db.execute(
                    f"UPDATE {table} SET status='uncertain' WHERE project_id=? AND source_id=?",
                    (project, uncertain),
                ).rowcount == 1
            db.execute(
                """INSERT INTO work_runs(id,project_id,task_id,event_id,generation,status,
                   error,started_at,updated_at)
                   SELECT 'uncertain-fixture-run',project_id,task_id,id,?,'uncertain',
                   'fixture submission not confirmed',?,? FROM work_events WHERE project_id=? AND source_id=?""",
                (old_generation, now, now, project, uncertain),
            )
        def uncertain_run():
            with sqlite3.connect(data / "bridge.sqlite3", timeout=10) as db:
                return db.execute("SELECT * FROM work_runs WHERE id='uncertain-fixture-run'").fetchone()

        uncertain_run_before = uncertain_run()
        assert uncertain_run_before is not None
        pending_before, uncertain_before = events()[pending], events()[uncertain]
        assert pending_before["status"] == "queued" and uncertain_before["status"] == "uncertain"
        (base / "startup-credential").touch(mode=0o600)
        time.sleep(0.5)
        assert launches() == [start] and not state()["ready"], "original AGY process noticed a startup-only credential"
        for _ in range(3):
            api(f"/projects/{project}/agent/repair", {})
        smoke.wait_for(lambda: len(launches()) >= 2, "AGY recovery never launched a fresh process", timeout=40)
        old_launch, replacement = launches()
        assert old_launch == start and old_launch["pid"] != replacement["pid"]
        assert replacement["credential_at_start"] and replacement["native"] == NATIVE, replacement
        continuing = state()
        assert continuing["generation"] != old_generation and continuing["native_session"] == NATIVE
        assert continuing["execution"].get("mcp_registered") is not True
        assert continuing["cli_transition"]["holds_input"] is True
        assert events()[pending] == pending_before and events()[uncertain] == uncertain_before
        assert not (base / "delivered").exists(), "work delivered before replacement registration"

        (base / "release-registration").touch()
        smoke.wait_for(
            lambda: (base / "registered").exists() and state()["ready"]
            and not state()["cli_transition"]["holds_input"],
            "fresh AGY did not register and release the delivery lock", timeout=35,
        )
        time.sleep(1)
        assert len(launches()) == 2 and state()["native_session"] == NATIVE
        assert events()[uncertain] == uncertain_before and uncertain_run() == uncertain_run_before
        delivered = (base / "delivered").read_text() if (base / "delivered").exists() else ""
        assert uncertain not in delivered
        print(json.dumps({
            "passed": True, "herdr": "0.9.5", "fixture_only": True,
            "initial_starts": 1, "replacement_starts": 1,
            "fresh_process_read_startup_credential": True, "native_resume_preserved": True,
            "pending_and_uncertain_preserved_during_lock": True, "uncertain_not_replayed": True,
            "mcp_contract": 1, "api_protocol": 7,
        }, sort_keys=True))
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=12)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
        subprocess.run([str(herdr), "--session", session, "server", "stop"],
                       env=env, capture_output=True, timeout=10)
        fixture.server.shutdown()
        log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    herdr = Path(shutil.which("herdr")).resolve()
    assert subprocess.check_output([herdr, "--version"], text=True, timeout=5).strip() == "herdr 0.9.5"
    with tempfile.TemporaryDirectory(prefix="db-agy-", dir="/tmp") as folder:
        base = Path(folder).resolve() / "h"
        try:
            run(args.binary.resolve(), herdr, base)
        finally:
            if args.evidence and (base / "daemon.log").exists():
                shutil.copyfile(base / "daemon.log", args.evidence)


if __name__ == "__main__":
    main()
