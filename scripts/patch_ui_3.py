p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ═══════════ 模式框架 + 圈选 ═══════════

old = '''/* ───────── 启动 ───────── */'''
new = '''/* ───────── 模式框架 ───────── */
function setMode(m){
  mode=m;localStorage.setItem('litmus_mode',m);
  document.body.dataset.mode=m;
  document.querySelectorAll('#mode-seg button').forEach(b=>b.classList.toggle('on',b.dataset.mode===m));
  if(m==='organize'){
    document.querySelector('[data-view=kanban]')?.click();
    toast('整理模式：空白拖拽圈选，批量移动/打标签/删除');
  }
  if(m==='learn'){
    if(current){openDoc(current);}else{const d=graph.nodes.find(n=>n.kind!=='asset');if(d)openDoc(d.path);}
    $('d-body').classList.add('view');
    toast('学习模式：正文阅读，Esc 返回 Studio');
  }
  if(m==='present'){openPresent();}
  if(m==='rag'){openPalette();}
  if(m==='online'){$('pub-btn').click();}
  if(m==='agent'){toast('Agent 模式：对话优先，画布退居背景');}
  if(m==='studio'){$('d-body').classList.remove('view');}
}
document.querySelectorAll('#mode-seg button').forEach(b=>b.onclick=()=>setMode(b.dataset.mode));
setMode(localStorage.getItem('litmus_mode')||'studio');

/* ───────── 展示模式 ───────── */
let pIdx=0;
function docList(){return graph.nodes.filter(n=>n.kind==='doc'||n.kind==='capture').map(n=>n.path)}
async function openPresent(){
  const list=docList();
  if(!list.length){toast('没有可展示的文档','err');setMode('studio');return;}
  pIdx=Math.max(0,list.indexOf(current));
  $('present-ov').classList.add('open');
  await showPresent();
}
async function showPresent(){
  const list=docList();
  const path=list[pIdx];
  try{
    const d=await api('/doc?path='+encodeURIComponent(path));
    $('p-title').textContent=d.title||path;
    $('p-idx').textContent=`${pIdx+1}/${list.length}`;
    $('p-frame').srcdoc=d.html;
  }catch(e){toast('展示加载失败：'+e.message,'err')}
}
$('p-next').onclick=()=>{const n=docList().length;if(n){pIdx=(pIdx+1)%n;showPresent()}};
$('p-prev').onclick=()=>{const n=docList().length;if(n){pIdx=(pIdx-1+n)%n;showPresent()}};
$('p-exit').onclick=()=>{$('present-ov').classList.remove('open');setMode('studio')};

/* ───────── 圈选（整理模式） ───────── */
let lasso=null;
canvas.addEventListener('pointerdown',e=>{
  if(mode!=='organize'||e.target.closest('.node'))return;
  lasso={sx:e.clientX,sy:e.clientY};
  const el=$('lasso');el.style.display='block';
  el.style.left=e.clientX+'px';el.style.top=e.clientY+'px';el.style.width='0px';el.style.height='0px';
  canvas.setPointerCapture(e.pointerId);
});
canvas.addEventListener('pointermove',e=>{
  if(!lasso)return;
  const el=$('lasso');
  el.style.left=Math.min(lasso.sx,e.clientX)+'px';el.style.top=Math.min(lasso.sy,e.clientY)+'px';
  el.style.width=Math.abs(e.clientX-lasso.sx)+'px';el.style.height=Math.abs(e.clientY-lasso.sy)+'px';
});
canvas.addEventListener('pointerup',e=>{
  if(!lasso)return;
  const x0=Math.min(lasso.sx,e.clientX),x1=Math.max(lasso.sx,e.clientX);
  const y0=Math.min(lasso.sy,e.clientY),y1=Math.max(lasso.sy,e.clientY);
  lasso=null;$('lasso').style.display='none';
  if(x1-x0<12&&y1-y0<12)return;
  multiSel.clear();
  document.querySelectorAll('#nodes .node').forEach(el=>{
    const r=el.getBoundingClientRect();
    if(r.left<x1&&r.right>x0&&r.top<y1&&r.bottom>y0)multiSel.add(el.dataset.id);
  });
  document.querySelectorAll('#nodes .node').forEach(el=>el.classList.toggle('multi',multiSel.has(el.dataset.id)));
  $('batch-n').textContent=multiSel.size;
  $('batchbar').classList.toggle('show',multiSel.size>0);
});
function clearSel(){multiSel.clear();document.querySelectorAll('#nodes .node').forEach(el=>el.classList.remove('multi'));$('batchbar').classList.remove('show')}
$('b-clear').onclick=clearSel;
$('b-del').onclick=async()=>{
  if(!multiSel.size)return;
  if(!confirm(`删除选中的 ${multiSel.size} 篇文档？（文件真相将被移除）`))return;
  let ok=0;
  for(const id of multiSel){
    const n=graph.nodes.find(x=>x.id===id);
    if(n&&n.kind!=='asset'){try{await api('/doc?path='+encodeURIComponent(n.path),{method:'DELETE'});ok++}catch{}}
  }
  clearSel();toast(`已删除 ${ok} 篇`,'ok');await refreshAll();
};
$('b-move').onclick=async()=>{
  if(!multiSel.size)return;
  const dir=prompt('移动到目录（如 研究或 Sources/知识库；留空=根目录）','研究');
  if(dir===null)return;
  let ok=0;
  for(const id of multiSel){
    const n=graph.nodes.find(x=>x.id===id);
    if(!n||n.kind==='asset')continue;
    const fname=n.path.split('/').pop();
    const to=dir.trim()?`Notes/${dir.trim()}/${fname}`:`Notes/${fname}`;
    if(to===n.path)continue;
    try{await api('/doc/move',{method:'POST',json:{from:n.path,to}});ok++}catch{}
  }
  clearSel();toast(`已移动 ${ok} 篇`,'ok');await refreshAll();
};
$('b-tag').onclick=async()=>{
  if(!multiSel.size)return;
  const tag=prompt('标签名（写入文档正文 #标签）');
  if(!tag||!tag.trim())return;
  const t=tag.trim().replace(/^#/,'');
  let ok=0;
  for(const id of multiSel){
    const n=graph.nodes.find(x=>x.id===id);
    if(!n||n.kind==='asset')continue;
    try{
      const d=await api('/doc?path='+encodeURIComponent(n.path));
      const md=d.markdown||'';
      if(!md.includes('#'+t)){
        const updated=md.replace(/(\\n\\n)/,`\\n\\n#${t}\\n$1`);
        await api('/doc?path='+encodeURIComponent(n.path),{method:'PUT',body:updated,headers:{'content-type':'text/markdown'}});
        ok++;
      }
    }catch{}
  }
  clearSel();toast(`已为 ${ok} 篇打上 #${t}`,'ok');await refreshAll();
};

/* ───────── 启动 ───────── */'''
assert old in s
s = s.replace(old, new, 1)
open(p, 'w', encoding='utf-8').write(s)
print('modes + lasso done')
