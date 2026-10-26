import re

p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ═════════ CSS：动效 + 骨架 ═════════
s = s.replace('#toast{position:fixed;right:16px;bottom:44px;', '''/* ── 启动动效 ── */
#boot .ring{width:64px;height:64px;border-radius:50%;margin:0 auto 18px;position:relative;
  background:conic-gradient(from 0deg, transparent 0deg, var(--accent) 120deg, transparent 240deg);
  animation:spin 1.1s linear infinite}
#boot .ring::after{content:"3C";position:absolute;inset:5px;border-radius:50%;background:var(--bg);
  color:var(--accent);display:grid;place-items:center;font:800 17px var(--font-display)}
#boot-bar{position:relative;overflow:hidden}
#boot-bar::after{content:"";position:absolute;inset:0;background:linear-gradient(90deg,transparent,oklch(100% 0 0/.35),transparent);
  animation:sweep 1.3s var(--ease-out) infinite}
@keyframes sweep{from{transform:translateX(-100%)}to{transform:translateX(100%)}}
/* ── 骨架屏 ── */
.skel{height:11px;border-radius:6px;background:linear-gradient(90deg,var(--hover) 25%,var(--hover-strong) 37%,var(--hover) 63%);
  background-size:400% 100%;animation:shimmer 1.4s linear infinite;margin:9px 10px}
@keyframes shimmer{from{background-position:100% 0}to{background-position:-100% 0}}
.node.skel-node{width:var(--node-w);height:96px;border-radius:var(--r-md);border:1px solid var(--border);background:var(--node-bg);opacity:.55}
#toast{position:fixed;right:16px;bottom:44px;''', 1)

# 启动遮罩换成动效环
s = s.replace('''    <div style="width:52px;height:52px;border-radius:17px;background:var(--accent);color:var(--on-accent);display:grid;place-items:center;font:800 20px var(--font-display);margin:0 auto 16px">3C</div>''',
'''    <div class="ring"></div>''', 1)

# ═════════ JS：缓存 + 分级加载 ═════════
s = s.replace("let browsePath='Notes';\nlet canvasState='idle';", '''let browsePath='Notes';
let canvasState='idle';
/* ── 本地缓存（stale-while-revalidate）：下次打开秒出，再后台校准 ── */
const CACHE_KEY='litmus_cache_v1';
function cacheGet(k){try{return (JSON.parse(localStorage.getItem(CACHE_KEY)||'{}'))[k]}catch{return null}}
function cacheSet(k,v){try{const o=JSON.parse(localStorage.getItem(CACHE_KEY)||'{}');o[k]=v;localStorage.setItem(CACHE_KEY,JSON.stringify(o))}catch{}}
/* ── 侧栏骨架 ── */
function railSkeleton(n=8){
  const box=$('rail-list');
  box.innerHTML=Array.from({length:n},()=>'<div class="skel" style="width:'+(55+Math.random()*35)+'%"></div>').join('');
}
/* ── 画布骨架 ── */
function canvasSkeleton(n=6){
  const wrap=$('nodes');wrap.innerHTML='';
  for(let i=0;i<n;i++){
    const el=document.createElement('div');el.className='node skel-node';
    el.style.left=(140+(i%3)*366)+'px';el.style.top=(150+Math.floor(i/3)*196)+'px';
    wrap.appendChild(el);
  }
}''', 1)

# loadRail 改为 /tree（只目录）+ 文件按需
s = s.replace('''async function loadRail(){
  const box=$('rail-list');
  if(!box.dataset.loaded)box.innerHTML='<div style="padding:14px;color:var(--faint);font-size:12px;font-family:var(--font-mono)">正在读取目录…</div>';
  const r=await api('/docs');
  renderTree(r.docs);
  box.dataset.loaded='1';
  $('rail-count').textContent=r.docs.length;''','''async function loadRail(){
  // ① 缓存优先：上一次的目录树立即渲染
  const cachedTree=cacheGet('tree');
  if(cachedTree){try{renderTreeFromTree(cachedTree)}catch{}}
  else railSkeleton();
  // ② 拉轻量目录树（只文件夹+计数，百毫秒级）
  try{
    const t=await api('/tree');
    cacheSet('tree',t);
    renderTreeFromTree(t);
    $('rail-count').textContent=t.count;
  }catch(e){ if(!cachedTree) railSkeleton(4); }
  // ③ 后台静默：补全文件标题/标签（不阻塞界面）
  setTimeout(()=>loadDocsFull(),1200);''', 1)

# 新的侧栏渲染：目录树 + 展开时按需拉该文件夹文档
s = s.replace('''function renderTree(docs){''','''/* 用 /tree 的结构渲染（无文件标题）：文件夹秒开，文件展开时按需加载 */
function renderTreeFromTree(t){
  const box=$('rail-list');box.innerHTML='';
  const q=($('rail-search').value||'').trim().toLowerCase();
  const fullDocs=window.__fullDocs||[];   // 后台补全后可用
  function docsOf(path){
    return fullDocs.filter(d=>d.path.startsWith(path+'/') && !d.path.slice(path.length+1).includes('/'));
  }
  function leaf(d){
    const el=document.createElement('div');el.className='tree-leaf'+(current===d.path?' on':'');el.draggable=true;el.title=d.path;
    const tags=(d.tags||[]).slice(0,3).map(x=>`<span class="tagchip" style="background:${tagColor(x)}">${x}</span>`).join('');
    el.innerHTML=`<span class="nm"></span>${tags}`;
    el.querySelector('.nm').textContent=d.title||d.path.split('/').pop();
    el.onclick=()=>openDoc(d.path);
    el.ondragstart=e=>{e.dataTransfer.setData('text/litmus-path',d.path)};
    return el;
  }
  function row(name,count,path){
    const f=document.createElement('div');f.className='tree-folder';
    const r=document.createElement('div');r.className='tree-row';
    r.innerHTML=`<svg class="chev" viewBox="0 0 24 24"><path d="m9 6 6 6-6 6"/></svg><span class="nm"></span><span class="ct">${count}</span>`;
    r.querySelector('.nm').textContent=name;
    r.onclick=async()=>{
      const willOpen=!f.classList.contains('open');f.classList.toggle('open',willOpen);
      if(r.querySelector('.chev'))r.querySelector('.chev').style.transform=willOpen?'rotate(90deg)':'';
      if(willOpen&&!f.dataset.loaded){
        const kids=document.createElement('div');kids.className='tree-kids';
        (path.node?.dirs||[]).forEach(ch=>kids.appendChild(row(ch.name,ch.count,{path:ch.path,node:ch})));
        // 文件：有缓存就直接列，否则拉该文件夹（只这一层）
        const local=docsOf(path.path);
        if(local.length){local.forEach(d=>kids.appendChild(leaf(d)))}
        else{
          const loading=document.createElement('div');loading.className='tree-row';
          loading.innerHTML='<span class="nm" style="color:var(--faint)">载入文件…</span>';kids.appendChild(loading);
          api('/browse?path='+encodeURIComponent(path.path)).then(b=>{
            loading.remove();
            const cached=JSON.parse(localStorage.getItem('litmus_dir_'+path.path)||'null')||b;
            (cached.docs||[]).forEach(d=>kids.appendChild(leaf(d)));
          }).catch(()=>loading.remove());
        }
        f.appendChild(kids);f.dataset.loaded='1';
      }
    };
    r.ondragover=e=>{e.preventDefault();r.classList.add('drop-hot')};
    r.ondragleave=()=>r.classList.remove('drop-hot');
    r.ondrop=async e=>{
      e.preventDefault();r.classList.remove('drop-hot');
      const from=e.dataTransfer.getData('text/litmus-path');if(!from)return;
      const fname=from.split('/').pop();const to=`Notes/${path.path.replace(/^Notes\\//,'')}/${fname}`;
      if(to===from)return;
      try{await api('/doc/move',{method:'POST',json:{from,to}});toast(`${from} → ${to}`,'ok');await refreshAll();if(current===from)openDoc(to);}
      catch(err){toast('移动失败：'+err.message,'err');}
    };
    const w=document.createElement('div');w.appendChild(r);
    f.appendChild(w);return f;
  }
  (t.dirs||[]).forEach(d=>box.appendChild(row(d.name,d.count,{path:d.path,node:d})));
  // 根目录直属文件（有缓存就列）
  docsOf('Notes').filter(d=>!d.path.slice(6).includes('/')).forEach(d=>box.appendChild(leaf(d)));
  if(!t.dirs?.length&&!t.files)box.innerHTML='<div style="padding:14px;color:var(--faint);font-size:12px">还没有文件夹</div>';
  updateDepth();
}
/* 后台静默补全：拉全量文档标题/标签，更新侧栏与检索本地库 */
async function loadDocsFull(){
  try{
    const r=await api('/docs');
    window.__fullDocs=r.docs;
    cacheSet('docs',r.docs);
    if(!($('rail-search').value||'').trim())renderTreeFromTree(cacheGet('tree')||{dirs:[]});
    $('rail-count').textContent=r.docs.length;
  }catch{}
}
function renderTree(docs){''', 1)

# 搜索时用本地全量（若已加载）而不是等后端
s = s.replace('''$('rail-search').addEventListener('input',()=>loadRail());''','''$('rail-search').addEventListener('input',()=>{
  if((window.__fullDocs||[]).length)renderTreeFromTree(cacheGet('tree')||{dirs:[]});
  else loadRail();
});''', 1)

# 启动：外壳立刻可用，数据分级进来
s = s.replace('''(async function init(){
  bootStep(10,'读取库配置…');
  try{await loadBoards();board=await api('/board?name='+encodeURIComponent(boardName));}catch{}
  applyThemePreset();
  bootStep(30,'读取目录…');
  await loadRail().catch(()=>{});
  bootStep(55,'读取当前文件夹…');
  await loadGraph().catch(()=>{});
  bootStep(75,'读取状态…');
  await Promise.all([loadStatus().catch(()=>{}),loadMeta().catch(()=>{})]);
  bootStep(92,'就绪');applyVP();fit();initPresence();bootDone();maybeOnboard();''','''(async function init(){
  applyThemePreset();
  // ① 先用缓存画出界面（毫秒级），不等任何请求
  const cs=cacheGet('status'); if(cs)applyStatus(cs);
  const cb=cacheGet('browse'); if(cb){try{renderBrowseCached(cb)}catch{}}
  canvasSkeleton(6);
  bootStep(45,'界面就绪');
  // ② 遮罩只等"外壳"，数据后台来
  setTimeout(bootDone,250);
  // ③ 分级并发：目录 → 当前文件夹 → 状态/配置
  loadRail().catch(()=>{});
  bootStep(60,'读取文件夹…');
  loadGraph().catch(()=>{});
  bootStep(80,'读取状态…');
  Promise.all([loadStatus().catch(()=>{}),loadMeta().catch(()=>{})]).then(()=>bootStep(100,'就绪'));
  applyVP();fit();
  setTimeout(()=>initPresence(),800);
  setTimeout(()=>maybeOnboard(),600);''', 1)

# 辅助：缓存渲染 + 状态应用
s = s.replace('''function bootStep(pct,msg){''','''function applyStatus(s){
  const m=$('sb-stats'); if(m)m.textContent=`${s.docs} docs · ${s.indexed} indexed · ${s.assets} assets`;
  const n=$('v-name'); if(n&&s.vault)n.textContent=s.vault;
  const c=$('sb-conn'); if(c)c.textContent='connected';
}
function renderBrowseCached(b){
  const nodes=[];
  (b.folders||[]).forEach(f=>nodes.push({id:'folder:'+f.path,kind:'folder',path:f.path,title:'📁 '+f.name,excerpt:`${f.count} 篇`,collection:f.name,tags:[]}));
  (b.docs||[]).forEach(d=>nodes.push({id:'doc:'+d.path,kind:(d.path.includes('/Sources/')?'capture':'doc'),path:d.path,title:d.title,excerpt:'',mtime:d.mtime,collection:(d.path.split('/').slice(1,-1).pop()||'(根)'),tags:[],score:d.score}));
  graph={nodes,edges:[]};board={nodes:{}};layoutMissing();renderBrowser(b);renderGraph();
}
function bootStep(pct,msg){''', 1)

open(p, 'w', encoding='utf-8').write(s)
print('staged loading + cache + effects')
