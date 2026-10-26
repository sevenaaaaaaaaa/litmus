import re

p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ═══════════ CSS：加载/空/错误状态 + 提示条 + 首屏引导 ═══════════
s = s.replace('#toast{position:fixed;right:16px;bottom:44px;', '''/* ── 画布初始化状态 ── */
#canvas-state{position:fixed;left:50%;top:50%;transform:translate(-50%,-50%);z-index:26;display:none;
  max-width:420px;padding:26px;border-radius:var(--r-lg);border:1px solid var(--border);
  background:var(--surface-strong);backdrop-filter:blur(18px);box-shadow:var(--shadow);text-align:center}
#canvas-state.on{display:block;animation:nodeIn .35s var(--ease-spring) both}
#canvas-state h3{margin:0 0 6px;font-family:var(--font-display);font-size:17px}
#canvas-state p{margin:0 0 14px;color:var(--muted);font-size:13px;line-height:1.7}
#canvas-state .row{display:flex;gap:8px;justify-content:center;flex-wrap:wrap}
.spinner{width:26px;height:26px;border-radius:50%;border:2.5px solid var(--border);border-top-color:var(--accent);
  margin:0 auto 12px;animation:spin .8s linear infinite}
/* ── 画布提示条（首次显示，可关闭） ── */
#hintbar{position:fixed;left:50%;transform:translateX(-50%);bottom:38px;z-index:24;display:flex;align-items:center;gap:10px;
  padding:6px 14px;border-radius:999px;border:1px solid var(--border);background:var(--surface);
  backdrop-filter:blur(12px);font:12px var(--font-mono);color:var(--muted);box-shadow:var(--shadow-sm)}
#hintbar b{color:var(--fg);font-weight:600}
#hintbar .x{cursor:pointer;color:var(--faint);padding:0 2px}
/* ── 首屏引导 ── */
#onboard{position:fixed;inset:0;z-index:250;display:none;place-items:center;background:color-mix(in oklab,var(--bg) 82%,transparent);backdrop-filter:blur(6px)}
#onboard.on{display:grid}
.ob-card{width:min(560px,92vw);padding:30px;border-radius:var(--r-lg);border:1px solid var(--border);background:var(--surface-strong);box-shadow:var(--shadow)}
.ob-card h2{margin:0 0 6px;font-family:var(--font-display);font-size:23px}
.ob-card .sub{color:var(--muted);font-size:13.5px;margin-bottom:22px}
.ob-grid{display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-bottom:20px}
.ob-item{padding:14px;border-radius:var(--r-md);border:1px solid var(--border);background:var(--surface);cursor:pointer;transition:all .2s var(--ease-spring);text-align:left}
.ob-item:hover{border-color:var(--accent);transform:translateY(-3px);box-shadow:var(--shadow-sm)}
.ob-item .t{font-weight:700;font-size:13.5px;margin-bottom:4px}
.ob-item .d{color:var(--faint);font-size:11.5px;line-height:1.5}
.ob-keys{display:flex;gap:14px;flex-wrap:wrap;color:var(--faint);font-size:11.5px;font-family:var(--font-mono)}
.ob-keys kbd{border:1px solid var(--border);border-radius:6px;padding:1px 6px;color:var(--muted)}
#toast{position:fixed;right:16px;bottom:44px;''', 1)

# ═══════════ HTML ═══════════
s = s.replace('<div id="crumb"></div>', '''<div id="crumb"></div>
<div id="canvas-state"></div>
<div id="hintbar"></div>
<div id="onboard"><div class="ob-card">
  <h2>欢迎来到 Litmus Studio</h2>
  <div class="sub">你的知识是文件，是画布，也是 agent 的工具。选一个起点开始：</div>
  <div class="ob-grid">
    <div class="ob-item" data-ob="topic"><div class="t">⚡ 采集主题</div><div class="d">输入一个主题，自动从维基/HN/arXiv 建库</div></div>
    <div class="ob-item" data-ob="new"><div class="t">＋ 新建文档</div><div class="d">从零开始写一篇</div></div>
    <div class="ob-item" data-ob="conn"><div class="t">🔌 连接 MCP</div><div class="d">把 Notion/飞书/内部系统接进来</div></div>
  </div>
  <div class="ob-keys">
    <span><kbd>⌘K</kbd> 命令与检索</span>
    <span><kbd>Ctrl+Shift+M</kbd> 随手记</span>
    <span><kbd>双击</kbd> 文件夹下钻</span>
    <span><kbd>拖空白</kbd> 平移</span>
  </div>
</div></div>''', 1)

# ═══════════ JS：初始化状态机 ═══════════
s = s.replace("let browsePath='Notes';", "let browsePath='Notes';\nlet canvasState='idle';")

s = s.replace('''async function loadGraph(){
  // 分层浏览：只加载当前文件夹（子文件夹 + 高频文档），不再一次铺开全库
  const b=await api('/browse?path='+encodeURIComponent(browsePath));''','''/* 画布状态：loading / empty / error / ready —— 用户永远知道"现在在发生什么" */
function setCanvasState(kind,info={}){
  canvasState=kind;
  const el=$('canvas-state');
  if(kind==='ready'){el.classList.remove('on');el.innerHTML='';return}
  if(kind==='loading'){
    el.innerHTML=`<div class="spinner"></div><h3>正在打开 ${info.name||browsePath}</h3><p>加载子文件夹与高频文档…</p>`;
  }else if(kind==='empty'){
    el.innerHTML=`<h3>这个文件夹是空的</h3><p>${browsePath}<br>没有子文件夹，也没有文档。</p>
      <div class="row">
        <button class="act" data-a="new">＋ 新建文档</button>
        <button class="act" data-a="topic">⚡ 采集主题</button>
        <button class="act" data-a="up">↑ 返回上层</button>
      </div>`;
  }else if(kind==='error'){
    el.innerHTML=`<h3>加载失败</h3><p>${info.msg||'未知错误'}</p>
      <div class="row"><button class="act" data-a="retry">重试</button><button class="act" data-a="home">回到库根</button></div>`;
  }
  el.classList.add('on');
  el.querySelectorAll('[data-a]').forEach(b=>b.onclick=()=>{
    const a=b.dataset.a;
    if(a==='new')newDoc();
    if(a==='topic')$('e-topic')?$('e-topic').click():openPalette();
    if(a==='up'){browsePath=browsePath.split('/').slice(0,-1).join('/')||'Notes';loadGraph();}
    if(a==='retry')loadGraph();
    if(a==='home'){browsePath='Notes';loadGraph();}
  });
}
/* 画布提示条：首次显示，可关（记住选择） */
function showHint(){
  const bar=$('hintbar');
  if(localStorage.getItem('litmus_hint_off')==='1'){bar.innerHTML='';return}
  bar.innerHTML=`<b>双击</b>文件夹进入 · <b>拖空白</b>平移 · <b>⌘滚轮</b>缩放 · <b>⌘K</b>检索 <span class="x" title="不再显示">✕</span>`;
  bar.querySelector('.x').onclick=()=>{localStorage.setItem('litmus_hint_off','1');bar.innerHTML='';};
}

async function loadGraph(){
  showHint();
  setCanvasState('loading',{name:browsePath.split('/').pop()});
  let b;
  try{
    b=await api('/browse?path='+encodeURIComponent(browsePath));
  }catch(e){
    setCanvasState('error',{msg:e.message});
    return;
  }''', 1)

# 空文件夹判定 + ready
s = s.replace('''  graph={nodes,edges:[]};
  board={nodes:{}};   // 分层视图不持久化位置，自动排布
  layoutMissing();renderBrowser(b);
  renderGraph();
}''','''  graph={nodes,edges:[]};
  board={nodes:{}};   // 分层视图不持久化位置，自动排布
  layoutMissing();renderBrowser(b);renderGraph();
  if(!nodes.length)setCanvasState('empty',{});
  else setCanvasState('ready');
}''', 1)

# 首屏引导（只显示一次）
s = s.replace('''(async function init(){''','''/* 首屏引导：仅首次访问 */
function maybeOnboard(){
  if(localStorage.getItem('litmus_onboarded')==='1')return;
  const el=$('onboard');
  if(!el)return;
  el.classList.add('on');
  el.querySelectorAll('[data-ob]').forEach(b=>b.onclick=()=>{
    localStorage.setItem('litmus_onboarded','1');
    el.classList.remove('on');
    const k=b.dataset.ob;
    if(k==='new')newDoc();
    if(k==='topic')openPalette();
    if(k==='conn')$('board-pill').dispatchEvent(new MouseEvent('dblclick'));
  });
  el.addEventListener('click',e=>{
    if(e.target===el){localStorage.setItem('litmus_onboarded','1');el.classList.remove('on');}
  });
}

(async function init(){''', 1)
s = s.replace('''  await refreshAll();applyVP();fit();initPresence();''','''  await refreshAll();applyVP();fit();initPresence();maybeOnboard();''', 1)

# 侧栏加载态 + 自动展开当前文档所在路径
s = s.replace('''async function loadRail(){
  const r=await api('/docs');
  renderTree(r.docs);
  $('rail-count').textContent=r.docs.length;''','''async function loadRail(){
  const box=$('rail-list');
  if(!box.dataset.loaded)box.innerHTML='<div style="padding:14px;color:var(--faint);font-size:12px;font-family:var(--font-mono)">正在读取目录…</div>';
  const r=await api('/docs');
  renderTree(r.docs);
  box.dataset.loaded='1';
  $('rail-count').textContent=r.docs.length;''', 1)

open(p, 'w', encoding='utf-8').write(s)
print('initialization states + onboarding')
