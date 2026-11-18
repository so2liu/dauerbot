"""Isolated prior-boot simulation with real HERDR 0.9.5/MCP and native-shaped shims.

No host reboot, live bot, vendor account, or real model turn is exercised.
Only this fixture's private execution journal receives simulated prior-boot data.
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
    evidence = {'scope': 'Simulated prior-boot private store; actual HERDR0.9.5, SDK WebSockets and authenticated MCP; deterministic Codex/Pi protocol shims, no vendor/model acceptance'}
    daemon = None
    cfg = None
    projects = []
    sample_stop = threading.Event()
    sample_thread = None
    native_samples = []
    session = 'reboot-' + uuid.uuid4().hex[:8]
    with tempfile.TemporaryDirectory(prefix='db-boot-', dir='/tmp') as folder:
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
        shim.write_text(f'''import json,os,runpy,sys
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

        def rpc(method, params):
            path = base / 'xdg/herdr/sessions' / session / 'herdr.sock'
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(5)
                conn.connect(str(path))
                conn.sendall((json.dumps({'id':'reboot-fixture', 'method':method, 'params':params}) + '\n').encode())
                result = json.loads(conn.makefile().readline())
                assert 'error' not in result, result
                return result['result']

        try:
            daemon = start(runtime)
            cfg = smoke.wait_for(lambda:json.loads((data / 'config.json').read_text()) if (data / 'config.json').exists() else None, 'no private daemon config')
            smoke.wait_for(lambda:api('/state'), 'private daemon did not start')
            for index, agent in enumerate(['codex', 'codex', 'pi', 'pi']):
                path = base / f'work{index}'
                path.mkdir()
                p = api('/projects', {'name':f'Prior boot {agent} {index}', 'path':str(path), 'agent':agent})
                app = f'cli_reboot_{index}'
                api(f"/projects/{p['id']}/bot", {'app_id':app, 'app_secret':'fixture-secret'}, method='PUT')
                smoke.wait_for(lambda:app in smoke.FIXTURE.connection_bots.values(), 'no private bot connection')
                api(f"/projects/{p['id']}/start", {})
                smoke.wait_for(lambda:state(p['id'])['ready'] and state(p['id'])['execution'].get('mcp_registered') and state(p['id'])['execution'].get('native_confirmed'), 'native fixture not ready', timeout=45)
                current = state(p['id'])
                warm = f'om_reboot_warm_{index}'
                smoke.FIXTURE.emit(smoke.message(warm, 'already submitted effect; never replay it'), app)
                smoke.wait_for(lambda:any(d['source'] == warm for d in deliveries(path)), 'warm effect missing')
                api(f"/projects/{p['id']}/pause", {'paused':True})
                pending = f'om_reboot_pending_{index}'
                smoke.FIXTURE.emit(smoke.message(pending, 'deliver once after saved native recovery'), app)
                api(f"/projects/{p['id']}/timers", {'name':'Fired before reboot', 'prompt':'held reboot timer', 'kind':'once', 'at':int(time.time()*1000)+50, 'purpose':'user'})
                smoke.wait_for(lambda:any(e['source_id'] == pending and e['status'] == 'queued' for e in api(f"/projects/{p['id']}/work")['events']) and any(e['source'] == 'timer' for e in api(f"/projects/{p['id']}/work")['events']), 'pending input/timer not persisted')
                projects.append({'id':p['id'], 'agent':agent, 'path':str(path), 'old':current, 'warm':warm, 'pending':pending})
            standby_path = base / 'standby'
            standby_path.mkdir()
            standby = api('/projects', {'name':'Standby stays asleep', 'path':str(standby_path), 'agent':'codex'})
            daemon.terminate()
            daemon.wait(timeout=15)
            daemon = None
            subprocess.run([cfg['herdr'], '--session', session, 'server', 'stop'], env=env, check=True, capture_output=True, timeout=10)
            smoke.wait_for(lambda:not (base / 'xdg/herdr/sessions' / session / 'herdr.sock').exists(), 'old runtime socket survived')
            for p in projects:
                try:
                    os.kill(p['old']['terminal_pid'], 0)
                    raise AssertionError('old native root survived private runtime shutdown')
                except ProcessLookupError:
                    pass
            # Every attempted resume of saved A reports a fork B until the
            # authenticated repair-only Agent receives a real /repair instruction.
            # A blind restart loop can never pass this fixture.
            (Path(projects[0]['path']) / '.fixture-fork-on-resume').write_text('persistent')
            old_boot = str(uuid.uuid4())
            with sqlite3.connect(data / 'bridge.sqlite3', timeout=10) as db:
                for index,p in enumerate(projects):
                    execution = dict(p['old']['execution'])
                    execution.pop('launch_started_at', None)
                    if index % 2:
                        execution['host_boot_id'] = old_boot
                    else:
                        execution.pop('host_boot_id', None)  # legacy journal hydration
                    db.execute('UPDATE projects SET execution=?,paused=0 WHERE id=?', (json.dumps(execution),p['id']))
                    db.execute('UPDATE work_executions SET created_at=1 WHERE project_id=? AND generation=?', (p['id'],p['old']['generation']))
                    db.execute("UPDATE work_runs SET status='submitted',finished_at=NULL WHERE project_id=? AND event_id IN(SELECT id FROM work_events WHERE source_id=?)", (p['id'],p['warm']))
                    db.execute("UPDATE work_events SET status='submitted' WHERE project_id=? AND source_id=?", (p['id'],p['warm']))
                    if p['agent'] == 'pi':
                        adapter = Path(execution['child_adapter'])
                        (adapter / 'children.json').unlink(missing_ok=True)
                        (adapter / 'coverage.unknown').write_text('Simulated prior-boot incomplete child coverage\n')
            monitor = bindir / 'herdr-monitor'
            starts = base / 'server-starts'
            monitor.write_text(f'''#!{sys.executable}
import os,sys
from pathlib import Path
a=sys.argv[1:]
if a[-1:]==["server"]:
 with Path({str(starts)!r}).open("a") as f:f.write("start\\n")
os.execv({cfg["herdr"]!r},[{cfg["herdr"]!r}]+a)
''')
            monitor.chmod(0o700)
            daemon = start(monitor)
            def observe_native():
                while not sample_stop.wait(0.05):
                    try:
                        p=state(projects[0]['id'])
                        with sqlite3.connect(data / 'bridge.sqlite3', timeout=2) as db:
                            journals=[row[0] for row in db.execute('SELECT native_session FROM work_executions WHERE project_id=?',[p['id']])]
                            sources=[json.loads(row[0]) for row in db.execute('SELECT source FROM cli_switch_requests WHERE project_id=?',[p['id']])]
                        native_samples.append({'native':p['native_session'],'generation':p['generation'],
                            'journals':journals,'sources':[row.get('native_session') for row in sources],
                            'targets':[row.get('runtime_target',{}).get('native_session') for row in sources]})
                    except Exception:
                        pass
            sample_thread=threading.Thread(target=observe_native,daemon=True)
            sample_thread.start()
            repair_project = projects[0]
            repair_path = Path(repair_project['path'])
            lane_ready = repair_path / '.fixture-repair-lane-ready'
            smoke.wait_for(lane_ready.exists, 'persistent native fork did not receive an authenticated repair-only Agent envelope', timeout=80)
            lane_proof = json.loads(lane_ready.read_text())
            lane_state = state(repair_project['id'])
            lane = lane_state['execution']['repair_lane']
            assert lane['mcp_registered'] is True and lane['generation'] == lane_proof['generation'], lane_state
            assert lane_state['ready'] is False and lane_state['cli_transition']['holds_input'] is True
            assert lane_state['execution'].get('mcp_registered') is not True and lane_state['execution'].get('native_confirmed') is not True
            assert lane_state['native_session'] == lane['expected_native'] == repair_project['old']['native_session']
            assert lane['observed_native'] == lane_proof['observed'] != lane['expected_native']
            forks = [json.loads(line) for line in (repair_path / '.fixture-native-forks').read_text().splitlines()]
            fork = next(row for row in forks if row['generation'] == lane['generation'])
            stale_credentials = json.loads(Path(fork['credentials']).read_text())

            def session_post(operation, body):
                request = urllib.request.Request(stale_credentials['url'] + '/api/projects/' + stale_credentials['project'] + '/' + operation,
                    data=json.dumps(body).encode(), headers={'Authorization':'Bearer ' + stale_credentials['token'],
                    'x-hfb-generation':stale_credentials['generation'],'Content-Type':'application/json'})
                try:
                    with urllib.request.urlopen(request, timeout=15) as response:
                        return response.status, json.load(response)
                except urllib.error.HTTPError as error:
                    return error.code, json.loads(error.read())

            registration = {'generation':lane['generation'],'terminal':fork['pane'],'mcp_contract':1}
            code, registered = session_post('register', registration)
            assert code == 200 and registered['repair_only'] is True, (code, registered)
            held = api(f"/projects/{repair_project['id']}/work")
            pending_event = next(e for e in held['events'] if e['source_id'] == repair_project['pending'])
            timers = [e for e in held['events'] if e['source'] == 'timer']
            assert pending_event['status'] == 'queued' and len(timers) == 1 and timers[0]['status'] == 'queued', held
            assert not any(r['event_id'] in {pending_event['id'],timers[0]['id']} for r in held['runs']), held
            assert len(api(f"/projects/{repair_project['id']}/timers")) == 1, 'repair-only schedule_run created a timer'
            assert (repair_path / '.fixture-fork-on-resume').read_text() == 'persistent'
            assert not (repair_path / '.fixture-native-repaired').exists()
            assert not any(d['generation'] == lane['generation'] for d in deliveries(repair_path))
            smoke.wait_for(lambda:any(m.get('fixture_reply_to') == repair_project['warm'] and
                'repair-only Agent is investigating' in m.get('content','') for m in smoke.FIXTURE.messages),
                'repair-only MCP diagnostic did not reach its Feishu topic')
            smoke.FIXTURE.emit(smoke.message('om_reboot_repair_0', '/repair fixture-repair-persistent-native-resume'), 'cli_reboot_0')
            smoke.wait_for(lambda:(repair_path / '.fixture-native-repaired').exists(), 'trusted /repair instruction did not reach the repair Agent', timeout=35)
            repaired = json.loads((repair_path / '.fixture-native-repaired').read_text())
            assert repaired['generation'] == lane['generation'] and repaired['original'] == repair_project['old']['native_session']
            assert not (repair_path / '.fixture-fork-on-resume').exists()
            evidence['repair_only_lane'] = {'original_native_preserved':True,'persistent_fork':True,
                'authenticated_mcp_registration':True,'topic_diagnostic':True,'schedule_denied':True,
                'ordinary_input_and_timer_held':True,'trusted_repair_control':True,'request':lane['request_id']}
            smoke.wait_for(lambda:all(state(p['id'])['cli_transition'] and state(p['id'])['cli_transition']['phase'] == 'ready' and state(p['id'])['execution'].get('mcp_registered') and state(p['id'])['execution'].get('native_confirmed') for p in projects), 'saved native reboot recovery stalled', timeout=100)
            assert starts.read_text().splitlines() == ['start']
            assert fork['expected']==projects[0]['old']['native_session'] and fork['observed']!=fork['expected']
            assert native_samples and all(row['native']==fork['expected'] and
                all(value in {'',fork['expected']} for value in row['journals']) and
                all(value in {None,'',fork['expected']} for value in row['sources'] + row['targets']) for row in native_samples), native_samples
            assert not any(row['generation']==fork['generation'] for row in deliveries(Path(projects[0]['path']))), fork
            evidence['fork_rejected_without_native_adoption']=True
            assert (repair_path / 'HANDOFF.md').read_text().find('<!-- dauerbot-switch-ready:' + repaired['nonce'] + ' -->') >= 0
            final_a = state(repair_project['id'])
            assert final_a['generation'] != lane['generation'] and not final_a['execution'].get('repair_lane')
            assert final_a['execution']['mcp_registered'] is True and final_a['execution']['native_confirmed'] is True
            stale_results = []
            for operation, payload in [('register', registration), ('outbox', {'to':'oc_fixture',
                'content':'Stale repair lane must never send this', 'request_id':'stale-repair-lane-effect',
                'reply_to_message_id':repair_project['warm']})]:
                code, rejected = session_post(operation, payload)
                assert code in {401,403,409}, (operation, code, rejected)
                stale_results.append({'operation':operation,'status':code})
            assert not any('Stale repair lane must never send this' in m.get('content','') for m in smoke.FIXTURE.messages)
            with sqlite3.connect(data / 'bridge.sqlite3', timeout=2) as db:
                assert db.execute("SELECT count(*) FROM messages WHERE source_id='stale-repair-lane-effect'").fetchone()[0] == 0
            evidence['repair_only_lane']['stale_authority_rejected'] = stale_results
            results = []
            for p in projects:
                current = state(p['id'])
                assert current['native_session'] == p['old']['native_session']
                assert current['generation'] != p['old']['generation']
                smoke.wait_for(lambda:sum(d['source'] == p['pending'] for d in deliveries(Path(p['path']))) == 1 and len(deliveries(Path(p['path']))) == 3, 'pending input/timer missing after reboot')
                work = api(f"/projects/{p['id']}/work")
                assert next(e for e in work['events'] if e['source_id'] == p['warm'])['status'] == 'uncertain'
                lines = deliveries(Path(p['path']))
                assert sum(d['source'] == p['warm'] for d in lines) == 1, lines
                assert next(d for d in lines if d['source'] == p['pending'])['generation'] == current['generation']
                timer_events = [e for e in work['events'] if e['source'] == 'timer']
                assert len(timer_events) == 1 and len([r for r in work['runs'] if r['event_id'] == timer_events[0]['id']]) == 1
                foreign = rpc('pane.get', {'pane_id':p['old']['execution']['pane_id']})['pane']
                assert foreign['terminal_id'] != p['old']['terminal'] and not foreign.get('tokens', {}).get('hfb_generation')
                text = rpc('pane.read', {'pane_id':foreign['pane_id'], 'source':'visible', 'strip_ansi':True})['read']['text']
                assert 'dauerbot_cli_' not in text and 'dauerbot_event' not in text, text
                results.append({'agent':p['agent'],'native_preserved':True,'new_generation':current['generation'],'foreign_shell_preserved':True,'queued_once':True,'timer_once':True,'uncertain_not_replayed':True})
                api(f"/projects/{p['id']}/stop", {})
            assert state(standby['id'])['terminal'] is None
            evidence.update(count=4, results=results, shared_server_starts=1, standby_untouched=True)
        except Exception as error:
            evidence['error'] = str(error)
            try:
                evidence['projects'] = [{k:p.get(k) for k in ['id','agent','generation','ready','paused','native_session','cli_transition','lifecycle']} for p in api('/state')['projects']]
                evidence['log'] = (data / 'daemon.log').read_text()
                evidence['fixture_errors'] = [f.read_text() for f in base.glob('work*/.fixture-native-state-error')]
            except Exception as diagnostic:
                evidence['diagnostic_error'] = str(diagnostic)
            raise
        finally:
            sample_stop.set()
            if sample_thread is not None:
                sample_thread.join(timeout=25)
                evidence['sampler_joined']=not sample_thread.is_alive()
            if daemon is not None:
                daemon.terminate()
                daemon.wait(timeout=15)
            if cfg:
                stopped = subprocess.run([cfg['herdr'], '--session', session, 'server', 'stop'], env=env, capture_output=True, timeout=10)
                evidence['private_runtime_stop'] = stopped.returncode
            log.close()
            smoke.FIXTURE.server.shutdown()
            smoke.FIXTURE.server.server_close()
            owned = []
            for file in base.glob('work*/.fixture-launches'):
                owned.extend(json.loads(line).get('launch',{}).get('pid') for line in file.read_text().splitlines())
            alive = []
            for pid in filter(None,owned):
                try:os.kill(pid,0);alive.append(pid)
                except ProcessLookupError:pass
            evidence['surviving_native_pids'] = alive
            remaining = []
            for line in subprocess.check_output(['ps','-axo','pid=,command='],text=True).splitlines():
                fields=line.strip().split(None,1)
                if len(fields)==2 and int(fields[0]) not in {os.getpid(),os.getppid()} and (str(base) in fields[1] or str(base.resolve()) in fields[1]):
                    remaining.append(int(fields[0]))
            evidence['remaining_fixture_processes'] = remaining
            result = ROOT / 'test-results/reboot-native-smoke.json'
            result.parent.mkdir(exist_ok=True)
            result.write_text(json.dumps(evidence,indent=2)+'\n')
    evidence['temporary_directory_removed'] = not base.exists()
    result.write_text(json.dumps(evidence,indent=2)+'\n')
    assert not evidence['surviving_native_pids'] and not evidence['remaining_fixture_processes'] and evidence['temporary_directory_removed']
    print(json.dumps(evidence))


if __name__ == '__main__':
    main()
