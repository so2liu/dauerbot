"""Private real HERDR/MCP fixture for cooperative immediate CLI transitions.

The target codex is a deterministic private shim, not a real model/login test.
No user configuration or runtime is touched.
"""
import argparse,json,os,shutil,socket,subprocess,sys,tempfile,threading,time,urllib.request
from pathlib import Path
from http.server import ThreadingHTTPServer
import smoke

ROOT=Path(__file__).resolve().parent.parent

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);args=parser.parse_args()
    binary=Path(args.binary).resolve();checks=[];daemon=None;session='sw-'+os.urandom(4).hex()
    with tempfile.TemporaryDirectory(prefix='db-sw-',dir='/tmp') as temp:
        base=Path(temp);home=base/'home';home.mkdir();(home/'.codex').mkdir();(home/'.codex/config.toml').write_text('');workspace=base/'work';workspace.mkdir();data=base/'data';data.mkdir();bindir=home/'bin';bindir.mkdir()
        # Login-shell initialization stays private and resolves only this shim.
        (home/'.profile').write_text('export PATH="'+str(bindir)+':$PATH"\n')
        wrapper=f'''#!{sys.executable}
import json,os,runpy,sys
if '--help' in sys.argv or '--version' in sys.argv:
 print('codex-cli 0.167.2');sys.exit(0)
a=sys.argv
command=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.command=')))
argv=json.loads(next(v.split('=',1)[1] for v in a if v.startswith('mcp_servers.herdr_feishu.args=')))
sys.argv=[{str(ROOT/'scripts/mock_agent.py')!r},command,argv[-1],'',a[-1],'codex']
runpy.run_path(sys.argv[0],run_name='__main__')
'''
        (bindir/'codex').write_text(wrapper);(bindir/'codex').chmod(0o700)
        runtime=bindir/'herdr';shutil.copyfile(shutil.which('herdr'),runtime);runtime.chmod(0o700)
        env={k:v for k,v in os.environ.items() if not k.startswith(('HERDR_','HC_','SSH_')) and k not in {'CODEX_HOME','CLAUDE_CONFIG_DIR','PI_CODING_AGENT_DIR','GEMINI_CLI_HOME'}};env.update(HOME=str(home),XDG_CONFIG_HOME=str(base/'xdg'),XDG_STATE_HOME=str(base/'state'),SHELL='/bin/sh',PATH=str(bindir)+':'+os.environ['PATH'])
        smoke.FIXTURE.server=ThreadingHTTPServer(('127.0.0.1',0),smoke.Handler);threading.Thread(target=smoke.FIXTURE.server.serve_forever,daemon=True).start()
        s=socket.socket();s.bind(('127.0.0.1',0));port=s.getsockname()[1];s.close()
        log=(data/'daemon.log').open('w')
        try:
            daemon=subprocess.Popen([str(binary),'--data-dir',str(data),'serve','--port',str(port),'--herdr',str(runtime),'--herdr-session',session,'--feishu-base',f'http://127.0.0.1:{smoke.FIXTURE.server.server_port}'],env=env,stdout=log,stderr=log)
            cfg=smoke.wait_for(lambda:json.loads((data/'config.json').read_text()) if (data/'config.json').exists() else None,'no fixture config')
            def api(path,payload=None,method=None):
                r=urllib.request.Request(f'http://127.0.0.1:{port}/api'+path,data=None if payload is None else json.dumps(payload).encode(),headers={'Authorization':'Bearer '+cfg['token'],'Content-Type':'application/json'},method=method or ('POST' if payload is not None else 'GET'))
                with urllib.request.urlopen(r,timeout=20) as response:return json.load(response)
            smoke.wait_for(lambda:api('/state'),'fixture daemon not ready')
            p=api('/projects',{'name':'Cooperative switch fixture','path':str(workspace),'agent':'custom','extra_args':[sys.executable,str(ROOT/'scripts/mock_agent.py'),'{bridge_exe}','{credentials}','{native_session}','{instructions}']});key=p['id']
            api(f'/projects/{key}/bot',{'app_id':'cli_fixture','app_secret':'fixture-secret'},'PUT')
            smoke.wait_for(lambda:smoke.FIXTURE.connections,'no private websocket')
            api(f'/projects/{key}/start',{})
            smoke.wait_for(lambda:api(f'/projects/{key}')['project']['ready'],'old fixture not ready')
            old=api(f'/projects/{key}')['project'];(workspace/'.fixture-switch-mode').write_text('hold')
            api(f'/projects/{key}/agent/transition',{'agent':'codex'})
            smoke.wait_for(lambda:(workspace/'.fixture-switch-held').exists(),'cooperative handoff not requested')
            before=next(p['cli_transition'] for p in api('/state')['projects'] if p['id']==key)
            again=api(f'/projects/{key}/agent/transition',{'agent':'codex'})
            assert again['cli_transition']['id']==before['id']
            (workspace/'.fixture-mcp-hold').write_text('hold selected CLI registration')
            (workspace/'.fixture-switch-mode').unlink()
            smoke.FIXTURE.emit(smoke.message('om_switch_steer','new input during explicit CLI handoff'))
            def steered():
                work=api(f'/projects/{key}/work');return any(e['source_id']=='om_switch_steer' and e['status'] in ('submitted','delivered') for e in work['events'])
            smoke.wait_for(steered,'handoff blocked immediate Feishu steering',timeout=12)
            assert api(f'/projects/{key}')['project']['generation']==old['generation']
            checks.append('same-target retry reuses durable request; live Feishu input steers old generation during handoff')
            smoke.wait_for(lambda:len((workspace/'.fixture-launches').read_text().splitlines())==2,'selected shim did not launch',timeout=35)
            pending=next(p for p in api('/state')['projects'] if p['id']==key)
            assert pending['agent']=='codex' and pending['cli_transition']['phase']=='starting' and pending['execution'].get('mcp_registered') is not True
            smoke.FIXTURE.emit(smoke.message('om_switch_postclaim','input after retirement boundary'))
            def held_input():
                state=api(f'/projects/{key}/work');event=next((e for e in state['events'] if e['source_id']=='om_switch_postclaim'),None)
                if not event:return None
                assert event['status']=='queued' and not any(r['event_id']==event['id'] for r in state['runs']),state
                return event
            smoke.wait_for(held_input,'postclaim input was not retained')
            time.sleep(2.2)
            held_input()
            (workspace/'.fixture-mcp-hold').unlink()
            def ready():
                p=next(p for p in api('/state')['projects'] if p['id']==key)
                return p if p['agent']=='codex' and p['ready'] and p['cli_transition']['phase']=='ready' else None
            current=smoke.wait_for(ready,'selected CLI did not start immediately',timeout=50)
            assert current['generation']!=old['generation'] and current['native_session']=='' and current['execution']['mcp_registered'] is True
            assert current['cli_transition']['nonce']!=before['nonce'] and current['cli_transition']['attempts']>=2
            launches=[json.loads(line) for line in (workspace/'.fixture-launches').read_text().splitlines()]
            assert len(launches)==2 and not launches[-1]['resume'] and launches[-1]['native']!=launches[0]['native'], launches
            assert 'user explicitly switched' in launches[-1]['instructions'] and 'do not replay already delivered' in launches[-1]['instructions']
            assert 'dauerbot-switch-ready:' in (workspace/'HANDOFF.md').read_text()
            checks.append('fresh cooperative nonce handoff starts selected CLI now with fresh native context and explicit handoff continuation instructions')
            work=api(f'/projects/{key}/work');events=[e for e in work['events'] if e['source_id']=='om_switch_steer'];assert len(events)==1
            assert len([r for r in work['runs'] if r['event_id']==events[0]['id']])==1
            def delivered_new():
                state=api(f'/projects/{key}/work');event=next((e for e in state['events'] if e['source_id']=='om_switch_postclaim'),None)
                if not event:return None
                runs=[r for r in state['runs'] if r['event_id']==event['id']]
                return runs if len(runs)==1 and runs[0]['generation']==current['generation'] else None
            smoke.wait_for(delivered_new,'postclaim input did not reach new registered generation once')
            checks.append('old input is not replayed; postclaim input stays unclaimed until selected-generation MCP registration and is then delivered once')
            # A terminal-free project has no old native state to wake merely to write a file.
            sleeping=base/'sleeping';sleeping.mkdir();p2=api('/projects',{'name':'Fresh sleeping switch','path':str(sleeping),'agent':'claude'});api(f'/projects/{p2["id"]}/agent/transition',{'agent':'codex'})
            fresh=smoke.wait_for(lambda:next((p for p in api('/state')['projects'] if p['id']==p2['id'] and p['agent']=='codex' and p['ready']),None),'sleeping CLI was not started now',timeout=40)
            l=[json.loads(line) for line in (sleeping/'.fixture-launches').read_text().splitlines()];assert len(l)==1 and 'fresh workspace' in l[0]['instructions']
            checks.append('sleeping project without HANDOFF starts selected CLI immediately with truthful fresh-workspace context')
            for p in [current,fresh]:api(f'/projects/{p["id"]}/stop',{})
        except Exception:
            evidence={'checks':checks}
            try:
                evidence['state']=api('/state')
                evidence['launches']=(workspace/'.fixture-launches').read_text()
                evidence['handoff']=(workspace/'HANDOFF.md').read_text() if (workspace/'HANDOFF.md').exists() else ''
                evidence['screen']=api(f'/projects/{key}/screen')
                evidence['daemon_log']=(data/'daemon.log').read_text()
            except Exception as error:evidence['diagnostic_error']=str(error)
            result=ROOT/'test-results/cli-switch-failure.json';result.parent.mkdir(exist_ok=True);result.write_text(json.dumps(evidence,indent=2)+'\n')
            raise
        finally:
            if daemon:daemon.terminate();daemon.wait(timeout=15)
            log.close();smoke.FIXTURE.server.shutdown()
            # Only this named private HERDR server; never the user's default server.
            try:subprocess.run([cfg['herdr'],'--session',session,'server','stop'],env=env,capture_output=True,timeout=5)
            except Exception:pass
            result=ROOT/'test-results/cli-switch-smoke.json';result.parent.mkdir(exist_ok=True);result.write_text(json.dumps({'checks':checks,'count':len(checks),'scope':'real HERDR/MCP/private deterministic CLI shim; no model/login acceptance'},indent=2)+'\n')
        print(json.dumps({'checks':checks,'count':len(checks)}))
if __name__=='__main__':main()
