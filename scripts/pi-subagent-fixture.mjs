/** Offline fixture against an installed Pi SDK loader and the pinned extension.
 * Child processes emit deterministic model-protocol events; no live model claim.
 * Run: node scripts/pi-subagent-fixture.mjs /path/to/pi-coding-agent
 */
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
if (process.argv.includes('--mode')) {
  await delay(process.argv.at(-1).includes('hold') ? 180 : 35);
  console.log(JSON.stringify({type:'message_end', message:{role:'assistant',content:[{type:'text',text:'fixture child complete'}],stopReason:'stop',usage:{input:1,output:1,cacheRead:0,cacheWrite:0,cost:{total:0}}}}));
  process.exit(0);
}
const sdkRoot = path.resolve(process.argv[2]);
const version = JSON.parse(fs.readFileSync(path.join(sdkRoot, 'package.json'))).version;
assert.equal(version,'1.5.1');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'db-pi-'));
process.env.PI_CODING_AGENT_DIR = path.join(tmp,'agent');
const managed = path.join(tmp,'managed');
fs.cpSync(path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../assets/pi-subagent'), managed, {recursive:true});
fs.writeFileSync(path.join(managed,'managed.json'), JSON.stringify({generation:'fixture-generation',incomplete:false}));
const {loadExtensions} = await import(pathToFileURL(path.join(sdkRoot,'dist/core/extensions/loader.js')));
let ext;
const ctx = {mode:'tui', cwd:tmp, model:{provider:'fixture',id:'chosen-model'},thinkingLevel:'low',isProjectTrusted:()=>true,
  sessionManager:{getSessionId:()=> 'fixture-parent',getSessionFile:()=>path.join(tmp,'parent.jsonl')}, ui:{confirm:async()=>true}};
const state = () => JSON.parse(fs.readFileSync(path.join(managed,'children.json')));
try {
 const result = await loadExtensions([path.join(managed,'managed.ts')], tmp);
 assert.deepEqual(result.errors,[]);
 assert.equal(result.extensions.length,1);
 ext = result.extensions[0];
 result.runtime.getSettings = () => ({});
 if(process.argv[3]) {
   const known=path.join(process.env.PI_CODING_AGENT_DIR,'extensions');
   fs.mkdirSync(known,{recursive:true});
   fs.copyFileSync(process.argv[3],path.join(known,'herdr-agent-state.ts'));
 }
 for (const fn of ext.handlers.get('session_start')) await fn({type:'session_start'},ctx);
 assert.equal(state().state,'quiescent','only the exact pinned HERDR observer can join known coverage');
 const tool = ext.tools.get('dauerbot_subagent').definition;
 for (const params of [
   {agent:'worker',task:'hold single'},
   {tasks:Array.from({length:7},(_,i)=>({agent:'worker',task:`hold parallel ${i}`}))},
   {chain:[{agent:'worker',task:'hold chain'},{agent:'worker',task:'hold chain {previous}'}]},
 ]) {
   const before = state().revision;
   let settled = false;
   const running = tool.execute('fixture-call',params,new AbortController().signal,undefined,ctx).then(x=>{settled=true;return x});
   await delay(15);
   assert.equal(state().state,'active'); assert.equal(settled,false,'foreground tool must await its children');
   for(let i=0;i<5;i++){await delay(15); assert.equal(state().state,'active','pending/parallel/chain gaps must stay active');}
   const outcome = await running;
   assert.equal(outcome.details.isError,false,JSON.stringify(outcome));
   assert.equal(state().state,'quiescent'); assert.equal(state().revision,before+2);
   assert.ok(outcome.details.results.every(x=>x.model==='fixture/chosen-model'),'selected parent model must be inherited');
 }
 const abort = new AbortController();
 const running = tool.execute('abort',{agent:'worker',task:'hold aborted'},abort.signal,undefined,ctx);
 await delay(20); abort.abort();
 const aborted = await running; assert.equal(aborted.details.isError,true); assert.equal(state().state,'quiescent');
 fs.mkdirSync(path.join(managed,'dispatch.lock'));
 await assert.rejects(tool.execute('blocked',{agent:'worker',task:'hold'},undefined,undefined,ctx));
 fs.rmdirSync(path.join(managed,'dispatch.lock'));
 fs.writeFileSync(path.join(managed,'retiring'),'claimed');
 await assert.rejects(tool.execute('retired',{agent:'worker',task:'hold'},undefined,undefined,ctx));
 assert.equal(state().state,'quiescent'); fs.unlinkSync(path.join(managed,'retiring'));
 // Unknown external MCP/plugin coverage is sticky for the adapter lifetime.
 const projectMcp=path.join(tmp,'.pi','mcp.json');
 fs.mkdirSync(path.dirname(projectMcp),{recursive:true});
 fs.writeFileSync(projectMcp,JSON.stringify({mcpServers:{other:{command:'unobserved-native-provider'}}}));
 for (const fn of ext.handlers.get('session_start')) await fn({type:'session_start'},ctx);
 assert.equal(state().state,'unknown','external MCP child providers are not covered');
 fs.unlinkSync(projectMcp);
 // A global plugin remains loaded and its own subagent tool is never replaced.
 const globalDir=process.env.PI_CODING_AGENT_DIR;
 fs.mkdirSync(path.join(globalDir,'extensions'),{recursive:true});
 const plugin=path.join(globalDir,'extensions','user.ts');
 fs.writeFileSync(plugin, `export default function(pi){pi.registerTool({name:'subagent',label:'User subagent',description:'preserved',parameters:{type:'object',properties:{}},async execute(){return {content:[{type:'text',text:'user plugin preserved'}]}}})}`);
 const preserved=await loadExtensions([plugin,path.join(managed,'managed.ts')],tmp);
 assert.deepEqual(preserved.errors,[]); assert.ok(preserved.extensions[0].tools.has('subagent'));
 const userResult=await preserved.extensions[0].tools.get('subagent').definition.execute();
 assert.equal(userResult.content[0].text,'user plugin preserved');
 for (const fn of ext.handlers.get('session_start')) await fn({type:'session_start'},ctx);
 assert.equal(state().state,'unknown','an unobserved plugin can launch children without registering tools');
 assert.ok(fs.existsSync(plugin),'global plugins must be preserved');
 fs.unlinkSync(plugin);
 for (const fn of ext.handlers.get('session_start')) await fn({type:'session_start'},ctx);
 assert.equal(state().state,'unknown','removing configuration does not unload an already running plugin');
 for(const fn of ext.handlers.get('session_shutdown')??[])await fn({type:'session_shutdown'},ctx);
 const reloaded=await loadExtensions([path.join(managed,'managed.ts')],tmp);
 assert.deepEqual(reloaded.errors,[]);
 reloaded.runtime.getSettings=()=>({}); ext=reloaded.extensions[0];
 for(const fn of ext.handlers.get('session_start'))await fn({type:'session_start'},ctx);
 assert.equal(state().state,'unknown','unknown coverage must survive adapter reload in the same execution generation');
 const unclean=path.join(tmp,'unclean'); fs.cpSync(managed,unclean,{recursive:true});
 fs.unlinkSync(path.join(unclean,'coverage.unknown'));
 const stale=await loadExtensions([path.join(unclean,'managed.ts')],tmp);
 assert.deepEqual(stale.errors,[]); stale.runtime.getSettings=()=>({});
 for(const fn of stale.extensions[0].handlers.get('session_start'))await fn({type:'session_start'},ctx);
 assert.equal(JSON.parse(fs.readFileSync(path.join(unclean,'children.json'))).state,'unknown','a previous ledger without clean shutdown cannot establish child coverage');
 for(const fn of stale.extensions[0].handlers.get('session_shutdown')??[])await fn({type:'session_shutdown'},ctx);
 console.log(JSON.stringify({pi:version,checks:'installed SDK loading; real subprocess await in single/parallel/chain; gaps; abort; native dispatch fence; model inheritance; global plugin preservation/unknown coverage/reload and unclean restart',liveModel:false}));
} finally {
 if(ext)for(const fn of ext.handlers.get('session_shutdown')??[])await fn({type:'session_shutdown'},ctx);
 fs.rmSync(tmp,{recursive:true,force:true});
}
