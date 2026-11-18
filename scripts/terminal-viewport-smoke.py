"""Actual private HERDR PTY viewport ownership, isolated native/MCP input smoke."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import sysconfig
import tempfile
import threading
import time
import urllib.request
import urllib.error
import uuid
from http.server import ThreadingHTTPServer

import smoke

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    binary = parser.parse_args().binary.resolve()
    evidence = {'scope': 'Private HERDR0.9.5 PTY geometry + native-shaped Codex/MCP; no live service, vendor account or real model turn'}
    evidence['binary_sha256'] = hashlib.sha256(binary.read_bytes()).hexdigest()
    daemon = None
    cfg = None
    streams = []
    readers = []
    session = 'viewport-' + uuid.uuid4().hex[:8]
    with tempfile.TemporaryDirectory(prefix='db-vp-', dir='/tmp') as folder:
        base = Path(folder)
        home = base / 'home'
        bindir = home / 'bin'
        bindir.mkdir(parents=True)
        (home / '.codex').mkdir()
        (home / '.pi/agent').mkdir(parents=True)
        (home / '.codex/config.toml').write_text('')
        (home / '.profile').write_text('export PATH="' + str(bindir) + ':$PATH"\n')
        data = base / 'data'
        data.mkdir()
        shim = bindir / 'native.py'
        shim.write_text(f'''import json,os,runpy,sys,signal
from pathlib import Path
kind=Path(sys.argv[1]).name;a=sys.argv[2:]
if '--version' in a or '--help' in a:print('1.5.1' if kind=='pi' else 'codex-cli 0.167.2');sys.exit(0)
if kind=='codex':
 entry=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.command=')))
 config=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.args=')))
 native=a[a.index('resume')+1] if 'resume' in a else '';instructions=a[-1];adapter=None
else:
 adapter=Path(a[a.index('--extension')+1]).parent
 config=json.loads((adapter/'managed.json').read_text())['mcp'];entry=config['command'];config=config['args']
 native=a[a.index('--session')+1] if '--session' in a else '';instructions=a[a.index('--append-system-prompt')+1]
def viewport(*_):
 size=os.get_terminal_size(0)
 Path('.fixture-viewport').write_text(json.dumps([size.columns,size.lines]))
signal.signal(signal.SIGWINCH,viewport);viewport()
metadata={{'argv':a,'pid':os.getpid(),'native_session_hook':'pi' if kind=='pi' else True,'child_adapter':str(adapter) if adapter else None,'assigned_native_id':a[a.index('--session-id')+1] if '--session-id' in a else None}}
sys.argv=[{str(ROOT / 'scripts/mock_agent.py')!r},entry,config[-1],native,instructions,kind,json.dumps(metadata)]
runpy.run_path(sys.argv[0],run_name='__main__')
''')
        launcher = bindir / 'launcher.c'
        launcher.write_text('#include <Python.h>\n#include <stdlib.h>\nint main(int argc,char **argv){\n'
            'char **args=calloc((size_t)argc+3,sizeof(char*));if(!args)return 70;\n'
            'args[0]=argv[0];args[1]=' + json.dumps(str(shim)) + ';args[2]=argv[0];\n'
            'for(int i=1;i<argc;i++)args[i+2]=argv[i];\n'
            'int result=Py_BytesMain(argc+2,args);free(args);return result;}\n')
        framework = sysconfig.get_config_var('PYTHONFRAMEWORK')
        link = ['-F', sysconfig.get_config_var('PYTHONFRAMEWORKPREFIX'), '-framework', framework] if framework else [
            '-L', sysconfig.get_config_var('LIBDIR'), '-lpython' + sysconfig.get_config_var('LDVERSION'),
            '-Wl,-rpath,' + sysconfig.get_config_var('LIBDIR'),
        ] + shlex.split(sysconfig.get_config_var('LIBS') or '') + shlex.split(sysconfig.get_config_var('SYSLIBS') or '')
        subprocess.run(['cc', '-I', sysconfig.get_path('include'), str(launcher), *link, '-o', str(bindir / 'codex')], check=True, capture_output=True, text=True, timeout=30)
        shutil.copyfile(bindir / 'codex', bindir / 'pi')
        (bindir / 'pi').chmod(0o700)
        runtime = bindir / 'herdr'
        shutil.copyfile(shutil.which('herdr'), runtime)
        runtime.chmod(0o700)
        env = {k:v for k,v in os.environ.items() if not k.startswith(('HERDR_', 'HC_', 'SSH_')) and k not in {'CODEX_HOME', 'CODEX_THREAD_ID', 'PI_CODING_AGENT_DIR', 'CLAUDE_CONFIG_DIR', 'GEMINI_CLI_HOME'}}
        env.update(HOME=str(home), CODEX_HOME=str(home / '.codex'), PI_CODING_AGENT_DIR=str(home / '.pi/agent'), XDG_CONFIG_HOME=str(base / 'xdg'), XDG_STATE_HOME=str(base / 'state'), SHELL='/bin/sh', PATH=str(bindir) + ':' + os.environ['PATH'])
        smoke.FIXTURE = smoke.Fixture()
        smoke.FIXTURE.server = ThreadingHTTPServer(('127.0.0.1', 0), smoke.Handler)
        threading.Thread(target=smoke.FIXTURE.server.serve_forever, daemon=True).start()
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
        sock.close()
        log = (data / 'daemon.log').open('w')

        def start(runtime_path):
            return subprocess.Popen([str(binary), '--data-dir', str(data), 'serve', '--port', str(port), '--herdr', str(runtime_path), '--herdr-session', session, '--feishu-base', f'http://127.0.0.1:{smoke.FIXTURE.server.server_port}'], env=env, stdout=log, stderr=log)

        def api(path, body=None, method=None):
            request = urllib.request.Request(f'http://127.0.0.1:{port}/api' + path, data=None if body is None else json.dumps(body).encode(), headers={'Authorization':'Bearer ' + cfg['token'], 'Content-Type':'application/json'}, method=method)
            try:
                return json.load(urllib.request.urlopen(request, timeout=20))
            except urllib.error.HTTPError as error:
                raise RuntimeError(path + ': ' + error.read().decode()) from error

        def state(project_id):
            return next(p for p in api('/state')['projects'] if p['id'] == project_id)

        def deliveries(path):
            f = path / '.fixture-deliveries'
            return [json.loads(line) for line in f.read_text().splitlines()] if f.exists() else []

        try:
            daemon = start(runtime)
            cfg = smoke.wait_for(lambda:json.loads((data / 'config.json').read_text()) if (data / 'config.json').exists() else None, 'no private daemon config')
            smoke.wait_for(lambda:api('/state'), 'private daemon did not start')
            workspace = base / 'work'
            workspace.mkdir()
            p = api('/projects', {'name':'Viewport fixture', 'path':str(workspace), 'agent':'codex'})
            project_id = p['id']
            app = 'cli_viewport_fixture'
            api(f"/projects/{project_id}/bot", {'app_id':app,'app_secret':'fixture-secret'}, method='PUT')
            smoke.wait_for(lambda:app in smoke.FIXTURE.connection_bots.values(), 'no private bot connection')
            api(f"/projects/{project_id}/start", {})
            smoke.wait_for(lambda:state(project_id)['ready'] and state(project_id)['execution'].get('native_confirmed'), 'native fixture not ready', timeout=45)
            original = state(project_id)
            descriptor = api(f"/projects/{project_id}/terminal")
            frames = []
            def attach(kind, cols, rows):
                args = [cfg['herdr'],'--session',session,'terminal','session',kind,descriptor['terminal_id'],'--cols',str(cols),'--rows',str(rows)]
                if kind == 'control': args.append('--takeover')
                proc = subprocess.Popen(args, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
                streams.append(proc)
                def consume():
                    for line in proc.stdout:
                        value=json.loads(line)
                        if value.get('type')=='terminal.frame':frames.append((proc.pid,value['width'],value['height']))
                reader=threading.Thread(target=consume,daemon=True);reader.start();readers.append(reader)
                return proc
            def dimensions():
                try:return json.loads((workspace / '.fixture-viewport').read_text())
                except (FileNotFoundError,json.JSONDecodeError):return None
            def resized(proc, cols, rows):
                smoke.wait_for(lambda:dimensions()==[cols,rows] and (proc.pid,cols,rows) in frames, f'actual PTY/frame not {cols}x{rows}')
            def resize(proc, cols, rows):
                proc.stdin.write(json.dumps({'type':'terminal.resize','cols':cols,'rows':rows})+'\n');proc.stdin.flush()
                resized(proc,cols,rows)
            def no_keyboard_lease():
                work=api(f"/projects/{project_id}/work")
                assert not (work.get('lease') or {}).get('active'), work.get('lease')
                assert state(project_id)['native_session']==original['native_session']
            first=attach('control',57,13)
            resized(first,57,13)
            no_keyboard_lease()
            observer=attach('observe',120,40)
            smoke.wait_for(lambda:any(pid==observer.pid for pid,_,_ in frames),'observer had no frame')
            assert dimensions()==[57,13], dimensions()
            resize(first,76,18)
            no_keyboard_lease()
            source='om_viewport_prompt'
            smoke.FIXTURE.emit(smoke.message(source,'normal message while geometry lease held'), app)
            smoke.wait_for(lambda:sum(d['source']==source for d in deliveries(workspace))==1,'agent.prompt blocked by geometry lease',timeout=35)
            smoke.wait_for(lambda:any(source in json.dumps(m) for m in smoke.FIXTURE.messages),'MCP topic reply missing',timeout=15)
            no_keyboard_lease()
            second=attach('control',38,9)
            resized(second,38,9)
            first.wait(timeout=10)
            try:
                first.stdin.write(json.dumps({'type':'terminal.resize','cols':100,'rows':30})+'\n');first.stdin.flush()
            except (BrokenPipeError,ValueError):pass
            observer.terminate();observer.wait(timeout=10)
            resize(second,64,16)
            no_keyboard_lease()
            assert sum(d['source']==source for d in deliveries(workspace))==1
            evidence.update(passed=True,checks=[
                'actual PTY SIGWINCH dimensions and ANSI frame follow narrow/wide panel measurements',
                'second observer cannot constrain geometry; viewport has no Dauerbot keyboard lease',
                'normal agent.prompt plus MCP topic reply complete while geometry controller remains open',
                'new view displaces old owner; old stream exits and cannot resize new view',
                'native session preserved and ordinary input delivered once'])
            api(f"/projects/{project_id}/stop", {})
        except Exception as error:
            evidence['error']=str(error)
            evidence['daemon_log']=(data/'daemon.log').read_text()
            print(json.dumps({'error':str(error),'frames':frames,'dims':dimensions(),'streams':[(p.pid,p.poll()) for p in streams]}),file=sys.stderr)
            raise
        finally:
            for proc in streams:
                if proc.poll() is None:proc.terminate()
                proc.wait(timeout=10)
                for stream in (proc.stdin,proc.stdout,proc.stderr):
                    if stream:
                        try:stream.close()
                        except BrokenPipeError:pass
            for reader in readers:reader.join(timeout=5)
            if daemon is not None:
                daemon.terminate();daemon.wait(timeout=15)
            if cfg:
                stopped=subprocess.run([cfg['herdr'],'--session',session,'server','stop'],env=env,capture_output=True,timeout=10)
                evidence['private_runtime_stop']=stopped.returncode
            log.close();smoke.FIXTURE.server.shutdown();smoke.FIXTURE.server.server_close()
            alive=[]
            launch_file=base/'work/.fixture-launches'
            if launch_file.exists():
                for line in launch_file.read_text().splitlines():
                    pid=json.loads(line).get('launch',{}).get('pid')
                    if pid:
                        try:os.kill(pid,0);alive.append(pid)
                        except ProcessLookupError:pass
            evidence['surviving_native_pids']=alive
            evidence['streams_closed']=all(p.poll() is not None for p in streams)
            evidence['readers_joined']=all(not r.is_alive() for r in readers)
            result=ROOT/'test-results/terminal-viewport-smoke.json';result.parent.mkdir(exist_ok=True)
            result.write_text(json.dumps(evidence,indent=2)+'\n')
    evidence['temporary_directory_removed']=not base.exists()
    result.write_text(json.dumps(evidence,indent=2)+'\n')
    assert not alive and evidence['streams_closed'] and evidence['readers_joined'] and evidence['temporary_directory_removed']
    print(json.dumps(evidence))


if __name__ == '__main__':
    main()
