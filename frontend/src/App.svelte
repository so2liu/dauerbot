<script lang="ts">
  import { onMount } from 'svelte';
  type Project = { id:string; name:string; path:string; agent:string; chat_id:string; paused:boolean; terminal:string|null; ready:boolean; native_session:string; extra_args:string[] };
  type Message = { id:string;direction:string;source_id:string;content:string;status:string;error:string;attachment:Record<string,string>;created_at:number };
  type Terminal = { state?:string; status?:{state?:string;kind?:string};foreground?:{name?:string} };
  type Snapshot = { projects:Project[];runtime:{core:string;core_error:string;feishu:string;feishu_error:string;terminals:Record<string,Terminal>;issues:Record<string,string>};settings:{app_id:string;has_secret:boolean;core_session:string};version:string };
  let token = $state(sessionStorage.getItem('hfb-token') || '');
  let loginToken = $state('');
  let snapshot = $state<Snapshot|null>(null);
  let selected = $state('');
  let detail = $state<{project:Project;messages:Message[]}|null>(null);
  let section = $state<'projects'|'settings'>('projects');
  let tab = $state<'activity'|'terminal'|'config'>('activity');
  let search = $state('');
  let editor = $state(false);
  let editing = $state(false);
  let draft = $state({name:'',path:'',agent:'codex',chat_id:'',native_session:'',args:''});
  let credential = $state({app_id:'',app_secret:''});
  let busy = $state('');
  let error = $state('');
  let notice = $state('');
  let localText = $state('');
  let outboundText = $state('');
  let screen = $state('');
  let connected = $state(false);
  let alive = true;
  let controller:AbortController|undefined;
  let refreshTimer:ReturnType<typeof setTimeout>|undefined;
  let noticeTimer:ReturnType<typeof setTimeout>|undefined;
  const names:Record<string,string>={codex:'Codex',claude:'Claude Code',pi:'Pi',agy:'Antigravity',custom:'自定义 CLI'};
  const connectionNames:Record<string,string>={unconfigured:'待配置',connecting:'正在连接',awaiting_events:'等待首条事件确认',receiving:'已收到事件',reconnecting:'正在重连'};
  const words:Record<string,string>={queued:'等待投递',submitting:'正在提交',submitted:'等待 Agent 确认',acknowledged:'已接收',pending:'等待发送',sending:'发送中',sent:'已送达',failed:'失败',uncertain:'结果待核对',dismissed:'已忽略',idle:'待命',done:'已完成',working:'工作中',blocked:'等待处理',error:'异常'};
  let projects = $derived(snapshot?.projects.filter(p=>`${p.name} ${p.path}`.toLowerCase().includes(search.toLowerCase())) ?? []);
  let current = $derived(snapshot?.projects.find(p=>p.id===selected));
  let terminal = $derived(current?.terminal ? snapshot?.runtime.terminals[current.terminal] : undefined);
  let activeCount = $derived(snapshot?.projects.filter(p=>p.terminal && snapshot?.runtime.terminals[p.terminal]?.state==='live').length ?? 0);
  let pendingCount = $derived(detail?.messages.filter(m=>['queued','submitting','submitted','pending','sending','uncertain','failed'].includes(m.status)).length ?? 0);
  function toast(text:string){notice=text;clearTimeout(noticeTimer);noticeTimer=setTimeout(()=>notice='',4000);}
  async function request(path:string,method='GET',body?:unknown){
    const response=await fetch(`/api${path}`,{method,headers:{Authorization:`Bearer ${token}`,...(body instanceof FormData?{}:{'Content-Type':'application/json'})},body:body===undefined?undefined:body instanceof FormData?body:JSON.stringify(body)});
    const value=await response.json();
    if(!response.ok){if(response.status===401){sessionStorage.removeItem('hfb-token');token='';}throw new Error(value.error||'请求失败');}
    return value;
  }
  async function refresh(){
    try{
      snapshot=await request('/state');connected=true;
      if(!selected && snapshot?.projects.length)selected=snapshot.projects[0].id;
      if(selected && !snapshot?.projects.some(p=>p.id===selected)){selected='';detail=null;}
      if(selected)detail=await request(`/projects/${selected}`);
    }catch(e){connected=false;error=(e as Error).message;}
  }
  async function live(){
    while(alive && token){
      controller=new AbortController();
      try{
        const r=await fetch('/api/events',{headers:{Authorization:`Bearer ${token}`},signal:controller.signal});
        if(!r.ok)throw new Error('状态连接中断');
        const reader=r.body!.getReader();
        const decoder=new TextDecoder();let buffer='';
        while(alive){
          const {value,done}=await reader.read();if(done)break;
          buffer+=decoder.decode(value,{stream:true});
          let split:number;
          while((split=buffer.indexOf('\n\n'))>=0){
            const event=buffer.slice(0,split);buffer=buffer.slice(split+2);
            if(event.includes('data:')){clearTimeout(refreshTimer);refreshTimer=setTimeout(refresh,150);}
          }
        }
      }catch{if(!alive)return;connected=false;}
      if(alive)await new Promise(r=>setTimeout(r,3000));
    }
  }
  async function connect(){
    token=loginToken;sessionStorage.setItem('hfb-token',token);error='';await refresh();
    if(snapshot){controller?.abort();void live();}
  }
  async function action(key:string,work:()=>Promise<unknown>){
    if(busy)return;busy=key;error='';
    try{await work();await refresh();}catch(e){error=(e as Error).message;}finally{busy='';}
  }
  async function select(p:Project){selected=p.id;section='projects';tab='activity';error='';detail=await request(`/projects/${p.id}`);}
  function edit(p?:Project){
    editing=!!p;
    draft=p?{name:p.name,path:p.path,agent:p.agent,chat_id:p.chat_id,native_session:p.native_session,args:JSON.stringify(p.extra_args)}:{name:'',path:'',agent:'codex',chat_id:'',native_session:'',args:''};
    editor=true;
  }
  async function saveProject(event:SubmitEvent){
    event.preventDefault();
    await action('save',async()=>{
      const extra_args=draft.args.trim()?JSON.parse(draft.args):[];
      if(!Array.isArray(extra_args)||extra_args.some(x=>typeof x!=='string'))throw new Error('启动参数须为 JSON 字符串数组');
      const result=await request(editing?`/projects/${selected}`:'/projects',editing?'PUT':'POST',{...draft,extra_args});
      selected=result.id;editor=false;toast(editing?'项目已更新':'项目已创建');
    });
  }
  async function command(operation:string){
    if(operation==='stop'&&!confirm('停止主会话会结束该 Agent 进程及关联子终端，项目和消息记录会保留。继续？'))return;
    await action(operation,async()=>{await request(`/projects/${selected}/${operation}`,'POST');toast(operation==='show'?'已请求在 HERDR 显示':'操作已完成');});
  }
  async function retry(m:Message){
    if(m.status==='uncertain'&&!confirm('请先核对终端或飞书是否已收到。重试可能重复执行任务或发送消息，确认重试？'))return;
    await action(m.id,()=>request(`/messages/${m.id}/retry`,'POST'));
  }
  async function dismiss(m:Message){
    if(!confirm('忽略这条消息并解除队列阻塞？'))return;
    await action(m.id,()=>request(`/messages/${m.id}/dismiss`,'POST'));
  }
  async function sendFile(event:Event){
    const input=event.currentTarget as HTMLInputElement;const file=input.files?.[0];if(!file)return;
    await action('upload',async()=>{
      const form=new FormData();form.append('file',file);
      const uploaded=await request(`/projects/${selected}/upload`,'POST',form);
      const result=await request(`/projects/${selected}/outbox`,'POST',{path:uploaded.path,request_id:crypto.randomUUID()});
      if(result.status!=='sent')throw new Error(result.error||'附件未确认送达，请查看消息记录');
      toast('附件已发送到飞书');
    });input.value='';
  }
  onMount(()=>{
    const fragment=new URLSearchParams(location.hash.slice(1));
    if(fragment.has('token')){token=fragment.get('token')!;sessionStorage.setItem('hfb-token',token);history.replaceState(null,'',location.pathname);}
    if(token){void refresh();void live();}
    return()=>{alive=false;controller?.abort();clearTimeout(refreshTimer);clearTimeout(noticeTimer);};
  });
</script>

{#if !token}
  <main class="login">
    <div class="login-card">
      <img src="/favicon.svg" alt="" class="brand-icon large"/>
      <p class="eyebrow">HERDR × FEISHU</p>
      <h1>连接你的项目工作台</h1>
      <p class="muted">在终端运行 <code>hfb ui</code> 安全打开，或输入本机配置文件中的管理令牌。</p>
      <form onsubmit={(e)=>{e.preventDefault();void connect();}}>
        <label>本机管理令牌<input type="password" bind:value={loginToken} placeholder="仅保存在当前浏览器会话" required/></label>
        <button class="primary" type="submit">连接工作台 →</button>
      </form>
      {#if error}<p class="error-text" role="alert">{error}</p>{/if}
      <small>服务仅监听 127.0.0.1，项目内容留在本机。</small>
    </div>
  </main>
{:else}
  <div class="shell">
    <aside>
      <a class="brand" href="/" onclick={(e)=>e.preventDefault()}><img src="/favicon.svg" alt=""/><span>herdr<span class="brand-light"> bridge</span><small>你的飞书 Agent 工作台</small></span></a>
      <div class="sidebar-label">工作空间</div>
      <button class:nav-active={section==='projects'} class="nav" onclick={()=>section='projects'}><span>▦</span>项目<span class="count">{snapshot?.projects.length??0}</span></button>
      <button class:nav-active={section==='settings'} class="nav" onclick={()=>{section='settings';credential.app_id=snapshot?.settings.app_id||'';}}><span>⚙</span>连接设置</button>
      <div class="sidebar-label project-label">项目 <button title="新建项目" onclick={()=>edit()}>＋</button></div>
      {#each snapshot?.projects??[] as p}
        <button class:chosen={selected===p.id&&section==='projects'} class="project-nav" onclick={()=>void select(p)}><span class="project-dot" class:running={p.ready&&!p.paused}></span><span>{p.name}</span>{#if p.paused}<small>暂停</small>{/if}</button>
      {/each}
      <div class="sidebar-footer"><span class="connection-dot" class:online={connected}></span>{connected?'本地服务在线':'正在重新连接'}<small>v{snapshot?.version??'0.1.0'} · 本机运行</small></div>
    </aside>
    <div class="workspace">
      <header><div class="breadcrumbs">工作空间 <span>/</span> {section==='settings'?'连接设置':'项目管理'}</div><div class="header-right"><span class="private-label">◈ 本地优先</span><button class="icon-button" title="刷新" onclick={()=>void refresh()}>↻</button></div></header>
      {#if error}<div class="error-banner" role="alert"><span>{error}</span><button onclick={()=>error=''} aria-label="关闭错误">×</button></div>{/if}
      {#if section==='settings'}
        <main class="page settings-page">
          <p class="eyebrow">CONNECTIONS</p><h1>连接设置</h1><p class="subtitle">将飞书对话接入本机，项目会话由 HERDR 持续托管。</p>
          <div class="settings-grid">
            <section class="panel">
              <div class="panel-heading"><h2>飞书机器人</h2><span class="badge">{snapshot?.settings.has_secret?'已配置':'待配置'}</span></div>
              <p class="muted">使用企业自建应用，启用机器人能力和长连接事件订阅。</p>
              <form onsubmit={(e)=>{e.preventDefault();void action('settings',async()=>{await request('/settings','PUT',credential);credential.app_secret='';toast('凭证已保存，监听器将重新连接');});}}>
                <label>App ID<input bind:value={credential.app_id} placeholder="cli_…" required/></label>
                <label>App Secret<input type="password" bind:value={credential.app_secret} autocomplete="new-password" placeholder={snapshot?.settings.has_secret?'已保存；留空保留现有密钥':'填写应用密钥'}/></label>
                <div class="button-row"><button type="submit" class="primary" disabled={!!busy}>保存配置</button><button type="button" class="secondary" disabled={!!busy} onclick={()=>void action('check',async()=>{const v=await request('/settings/check','POST');toast(v.message);})}>检查凭证</button></div>
              </form>
              <div class="setup-guide"><strong>飞书后台需要配置</strong><ol><li>事件接收方式选择「使用长连接接收事件」。</li><li>订阅 <code>im.message.receive_v1</code>。</li><li>开通发送消息、图片/文件资源权限；群聊全量收件需相应消息权限。</li><li>发布应用，将机器人加入项目群，再填写群的 <code>chat_id</code>。</li></ol></div>
            </section>
            <section class="panel status-panel"><h2>运行状态</h2><div class="status-row"><span>HERDR Core</span><span class="badge" class:good={snapshot?.runtime.core==='connected'}>{snapshot?.runtime.core==='connected'?'已连接':'未连接'}</span></div><div class="status-row"><span>Core session</span><code>{snapshot?.settings.core_session??'default'}</code></div><div class="status-row"><span>飞书监听器</span><span class="badge">{connectionNames[snapshot?.runtime.feishu??'']||'等待配置'}</span></div>
              {#if snapshot?.runtime.feishu_error}<p class="muted">{snapshot.runtime.feishu_error}</p>{/if}
              <div class="note">App、管理页面和后台服务独立运行。关闭此页面后，消息与主会话继续由后台处理。</div>
            </section>
          </div>
        </main>
      {:else}
        <main class="page">
          <div class="page-title"><div><p class="eyebrow">PROJECT WORKSPACE</p><h1>让项目保持在线<span class="heading-dot">.</span></h1><p class="subtitle">飞书里对话，终端里工作。每个项目都有自己的主会话。</p></div><button class="primary" onclick={()=>edit()}>＋ 新建项目</button></div>
          <div class="metrics"><div><span>项目总数</span><strong>{snapshot?.projects.length??'—'}</strong><small>独立上下文，清晰边界</small></div><div><span>运行中的主会话</span><strong>{activeCount}<i class="tiny-dot"></i></strong><small>随时打开 HERDR 接管</small></div><div><span>当前项目待处理</span><strong>{pendingCount}</strong><small>消息持久保存，可追溯</small></div><div><span>飞书机器人</span><strong class="metric-word">{snapshot?.settings.has_secret?'已配置':'待连接'}</strong><small>{snapshot?.settings.has_secret?'管理连接与事件订阅':'先在连接设置中添加凭证'}</small></div></div>
          <div class="project-area">
            <section class="project-list panel">
              <div class="list-header"><h2>我的项目</h2><span>{projects.length}</span></div>
              <label class="search"><span>⌕</span><input aria-label="搜索项目" bind:value={search} placeholder="搜索项目或目录…"/></label>
              {#each projects as p}
                <button class:selected={selected===p.id} class="project-card" onclick={()=>void select(p)}>
                  <div class="project-card-top"><span class="agent-avatar">{p.agent==='claude'?'✳':p.agent==='pi'?'π':p.agent==='agy'?'A':'›_'}</span><span class="badge" class:good={p.ready&&!p.paused}>{p.paused?'已暂停':p.ready?'已就绪':p.terminal?'待就绪':'未启动'}</span></div>
                  <strong>{p.name}</strong><span class="project-path" title={p.path}>{p.path}</span><div class="project-card-bottom"><span>{names[p.agent]}</span><span>{p.chat_id?'飞书已绑定':'待绑定飞书'} →</span></div>
                </button>
              {:else}<div class="list-empty">{search?'没有匹配项目':'创建第一个项目，让工作持续进行。'}</div>{/each}
              <button class="new-card" onclick={()=>edit()}>＋ 添加项目</button>
            </section>
            <section class="project-detail panel">
              {#if current}
                <div class="detail-heading"><div><p class="eyebrow">PROJECT / {names[current.agent]}</p><h2>{current.name}</h2><p class="detail-path">{current.path}</p></div><button class="icon-button" title="编辑项目" aria-label="编辑项目" onclick={()=>edit(current)}>⚙</button></div>
                <div class="session-strip"><span class="connection-dot" class:online={current.ready&&!current.paused}></span><span>{current.paused?'自动投递已暂停':terminal?.state==='finished'?'主会话已退出':current.ready?(words[terminal?.status?.state??'']||'主会话已就绪'):current.terminal?'等待 CLI 信任或 MCP 就绪':'主会话尚未启动'}</span>{#if current.terminal}<code>{current.terminal}</code>{/if}</div>
                {#if snapshot?.runtime.issues[current.id]}<p class="message-error" style="margin:12px 27px">投递暂缓：{snapshot.runtime.issues[current.id]}</p>{/if}
                <div class="session-actions">
                  {#if !current.terminal||terminal?.state==='finished'}<button class="primary" disabled={!!busy} onclick={()=>void command('start')}>▷ 启动会话</button><button class="secondary" disabled={!!busy||!current.native_session} onclick={()=>void command('resume')}>恢复原会话</button>{:else}<button class="secondary" disabled={!!busy} onclick={()=>void command('show')}>在 HERDR 打开 ↗</button><button class="secondary" disabled={!!busy} onclick={()=>void action('pause',()=>request(`/projects/${selected}/pause`,'POST',{paused:!current!.paused}))}>{current.paused?'继续投递':'暂停投递'}</button><button class="quiet danger" disabled={!!busy} onclick={()=>void command('stop')}>停止</button>{/if}
                </div>
                <div class="tabs"><button class:active={tab==='activity'} onclick={()=>tab='activity'}>消息活动 <span>{detail?.messages.length??0}</span></button><button class:active={tab==='terminal'} onclick={()=>{tab='terminal';void action('screen',async()=>{screen=JSON.stringify(await request(`/projects/${selected}/screen`),null,2);});}}>终端快照</button><button class:active={tab==='config'} onclick={()=>tab='config'}>项目配置</button></div>
                {#if tab==='activity'}
                  <div class="activity">
                    {#each detail?.messages??[] as m}
                      <article class="message"><div class="message-heading"><span class="direction" class:outbound={m.direction==='out'}>{m.direction==='in'?'↙ 收到':'↗ 发出'}</span><time>{new Date(m.created_at).toLocaleString('zh-CN',{month:'2-digit',day:'2-digit',hour:'2-digit',minute:'2-digit'})}</time><span class="badge" class:good={['sent','acknowledged'].includes(m.status)} class:bad={['failed','uncertain'].includes(m.status)}>{words[m.status]??m.status}</span></div><p class="message-content">{m.content||m.attachment.path||m.attachment.file_name||'附件'}</p>{#if m.error}<p class="message-error">{m.error}</p>{/if}
                        {#if ['failed','uncertain'].includes(m.status)}<button class="text-button" disabled={!!busy} onclick={()=>void retry(m)}>核对后重试</button>{/if}
                        {#if ['queued','submitted','uncertain','failed','pending'].includes(m.status)}<button class="text-button muted" disabled={!!busy} onclick={()=>void dismiss(m)}>忽略</button>{/if}
                      </article>
                    {:else}<div class="activity-empty"><span>↔</span><h3>消息会在这里汇合</h3><p>绑定飞书群并启动会话，即可把对话交给 Agent。<br/>也可以先发送一条本地消息验证投递。</p></div>{/each}
                  </div>
                  <div class="composer"><form onsubmit={(e)=>{e.preventDefault();void action('inject',async()=>{await request(`/projects/${selected}/messages`,'POST',{content:localText});localText='';toast('已加入主会话收件队列');});}}><label>向主会话发送本地消息<textarea bind:value={localText} placeholder="任务或补充说明…消息会先入队，再投递给主会话。" rows="2" required></textarea></label><div class="compose-footer"><small>本地测试入口 · 与飞书共用投递队列</small><button class="primary small" disabled={!!busy||!localText.trim()}>加入队列 ↑</button></div></form></div>
                {:else if tab==='terminal'}<div class="terminal-view"><div><span>只读快照 · 实时交互请打开 HERDR</span><button class="text-button" onclick={()=>void action('screen',async()=>screen=JSON.stringify(await request(`/projects/${selected}/screen`),null,2))}>刷新</button></div><pre>{screen||'尚无终端输出'}</pre></div>
                {:else}
                  <div class="config-view"><dl><dt>Agent</dt><dd>{names[current.agent]}</dd><dt>飞书聊天</dt><dd>{current.chat_id||'未绑定'}</dd><dt>原生会话 ID</dt><dd>{current.native_session||'启动后由 MCP 登记；也可在编辑中填写'}</dd><dt>项目标识</dt><dd>{current.id}</dd></dl><button class="secondary" onclick={()=>edit(current)}>编辑配置</button>
                  <div class="send-box"><h3>测试发送到飞书</h3><p class="muted">发送至该项目绑定的聊天，结果会记录在消息活动中。</p><textarea aria-label="飞书测试消息" bind:value={outboundText} placeholder="支持 Markdown，例如 **项目已连接**" rows="3"></textarea><div class="button-row"><button class="primary small" disabled={!!busy||!current.chat_id||!outboundText.trim()} onclick={()=>void action('send',async()=>{const v=await request(`/projects/${selected}/outbox`,'POST',{content:outboundText,request_id:crypto.randomUUID()});if(v.status!=='sent')throw new Error(v.error||'未确认送达');outboundText='';toast('飞书消息已发送');})}>发送 Markdown</button><label class="file-button">发送附件<input type="file" disabled={!!busy||!current.chat_id} onchange={(e)=>void sendFile(e)}/></label></div></div>
                  <div class="delete-area"><button class="text-button danger" onclick={()=>{if(confirm('删除项目及其消息记录？代码目录不会删除。'))void action('delete',()=>request(`/projects/${selected}`,'DELETE'));}}>删除项目</button></div></div>
                {/if}
              {:else}
                <div class="empty-project"><div class="empty-symbol">↗</div><p class="eyebrow">YOUR NEXT WORKSPACE</p><h2>给项目一个持续工作的空间</h2><p>选择代码目录与 Agent，绑定飞书群。<br/>工作在后台继续，你随时都能回来。</p><button class="primary" onclick={()=>edit()}>创建第一个项目</button><div class="empty-steps"><span>01 选择项目</span><span>02 连接飞书</span><span>03 启动会话</span></div></div>
              {/if}
            </section>
          </div>
          <footer>HERDR 托管会话 · Bridge 连接对话 <span>项目与消息保存在本机</span></footer>
        </main>
      {/if}
    </div>
  </div>
{/if}
{#if editor}
  <div class="modal-backdrop" role="presentation">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="project-title" tabindex="-1">
      <div class="modal-heading"><div><p class="eyebrow">PROJECT SETUP</p><h2 id="project-title">{editing?'编辑项目':'创建项目'}</h2></div><button class="icon-button" onclick={()=>editor=false} aria-label="关闭">×</button></div>
      <form onsubmit={(e)=>void saveProject(e)}>
        <label>项目名称<input bind:value={draft.name} placeholder="例如：支付服务" required maxlength="120"/></label>
        <label>本机代码目录<input bind:value={draft.path} placeholder="/Users/you/code/project" required/><small>使用绝对路径；不会复制或删除代码。</small></label>
        <label>主会话 Agent<select bind:value={draft.agent}>{#each Object.entries(names) as [value,label]}<option {value}>{label}</option>{/each}</select></label>
        <label>飞书 chat_id <span class="optional">可稍后绑定</span><input bind:value={draft.chat_id} placeholder="oc_…"/></label>
        <details><summary>高级设置</summary><label>原生会话 ID / 路径<input bind:value={draft.native_session} placeholder="用于恢复已有 CLI 对话"/></label><label>{draft.agent==='custom'?'完整 argv':'额外启动参数'}<textarea bind:value={draft.args} rows="2" placeholder='["--model", "模型名"]'></textarea><small>JSON 字符串数组。自定义 CLI 需自行接入生成的 MCP 配置。</small></label></details>
        <div class="modal-footer"><button type="button" class="secondary" onclick={()=>editor=false}>取消</button><button type="submit" class="primary" disabled={!!busy}>{busy==='save'?'保存中…':editing?'保存修改':'创建项目 →'}</button></div>
      </form>
    </div>
  </div>
{/if}
{#if notice}<div class="toast" role="status">✓ {notice}</div>{/if}
