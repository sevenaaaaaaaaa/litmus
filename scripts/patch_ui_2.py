p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ═══════════ JS ═══════════

# 1) 主题 + 模式状态初始化
old = "let themePref=localStorage.getItem('litmus_theme')||'auto';"
new = old + '''
let mode=localStorage.getItem('litmus_mode')||'studio';
let multiSel=new Set();'''
assert old in s
s = s.replace(old, new, 1)

# 2) 卡片渲染：tags + collection chip + 时间
old = '''    const meta=(view==='timeline'&&n.mtime)?`<span class="tag">${new Date(n.mtime*1000).toLocaleDateString('zh-CN')}</span>`:'';
    el.innerHTML=`<div class="n-head"><i class="n-kind"></i><span class="n-title">${title}</span></div><div class="n-path">${n.path||''}</div>${body}${meta?`<div class="n-tags">${meta}</div>`:''}`;'''
new = '''    const tags=(n.tags||[]).slice(0,4);
    const chips=tags.map(t=>`<span class="tagchip" style="background:${tagColor(t)}">${t}</span>`).join('');
    const metaBits=[];
    if(n.mtime)metaBits.push(`<span class="tag">${new Date(n.mtime*1000).toLocaleDateString('zh-CN')}</span>`);
    if(view!=='kanban'&&n.collection&&n.collection!=='附件')metaBits.push(`<span class="tag">${n.collection}</span>`);
    const metaRow=(tags.length||metaBits.length)?`<div class="n-tags">${chips}${metaBits.join('')}</div>`:'';
    el.innerHTML=`<div class="n-head"><i class="n-kind"></i><span class="n-title">${title}</span></div><div class="n-path">${n.path||''}</div>${body}${metaRow}`;
    el.addEventListener('mousemove',ev=>{
      if(matchMedia('(prefers-reduced-motion: reduce)').matches)return;
      const r=el.getBoundingClientRect();
      const rx=((ev.clientY-r.top)/r.height-.5)*-6, ry=((ev.clientX-r.left)/r.width-.5)*6;
      el.style.setProperty('--rx',rx.toFixed(2)+'deg');el.style.setProperty('--ry',ry.toFixed(2)+'deg');
    });
    el.addEventListener('mouseleave',()=>{el.style.setProperty('--rx','0deg');el.style.setProperty('--ry','0deg');});'''
assert old in s
s = s.replace(old, new, 1)

# 3) tagColor 助手 + 景深引擎
old = 'function applyVP(){$(\'viewport\').style.transform=`translate(${vp.x}px,${vp.y}px) scale(${vp.k})`;}'
new = old + '''
function tagColor(t){
  let h=0;for(const c of t)h=(h*31+c.codePointAt(0))%360;
  const dark=document.documentElement.dataset.theme==='dark';
  return `oklch(${dark?'72%':'52%'} .14 ${h})`;
}
/* 景深：离屏幕中心越远越小越淡（物理错落感） */
let depthQueued=false;
function queueDepth(){if(depthQueued)return;depthQueued=true;requestAnimationFrame(()=>{depthQueued=false;updateDepth()});}
function updateDepth(){
  if(view!=='canvas')return;
  const cx=innerWidth/2,cy=innerHeight/2;
  document.querySelectorAll('#nodes .node').forEach(el=>{
    const r=el.getBoundingClientRect();
    if(!r.width)return;
    const d=Math.hypot((r.left+r.width/2-cx)/innerWidth,(r.top+r.height/2-cy)/innerHeight);
    const sc=Math.max(.58,1-d*.75), op=Math.max(.5,1-d*.9);
    el.style.setProperty('--ds',multiSel.has(el.dataset.id)?'1':sc.toFixed(3));
    el.style.opacity=(el.dataset.id===selected||multiSel.has(el.dataset.id))?1:op.toFixed(3);
    el.style.filter=d>.34?`blur(${Math.min(2.4,(d-.34)*7).toFixed(2)}px)`:'';
  });
}'''
assert old in s
s = s.replace(old, new, 1)

# 4) pan/zoom 时触发景深
old = '''canvas.addEventListener('pointermove',e=>{if(!pan)return;vp.x=pan.ox+(e.clientX-pan.sx);vp.y=pan.oy+(e.clientY-pan.sy);applyVP();});'''
new = '''canvas.addEventListener('pointermove',e=>{if(!pan)return;vp.x=pan.ox+(e.clientX-pan.sx);vp.y=pan.oy+(e.clientY-pan.sy);applyVP();queueDepth();});'''
assert old in s
s = s.replace(old, new, 1)
old = '''  }else{vp.x-=e.deltaX;vp.y-=e.deltaY;}
  applyVP();
},{passive:false});'''
new = '''  }else{vp.x-=e.deltaX;vp.y-=e.deltaY;}
  applyVP();queueDepth();
},{passive:false});'''
assert old in s
s = s.replace(old, new, 1)

# 5) 侧栏树（文件夹/收展/彩色标签/拖动移动/搜索过滤）
old = '''async function loadRail(){
  const r=await api('/docs');const list=$('rail-list');list.innerHTML='';
  r.docs.slice().sort((a,b)=>(a.title||a.path).localeCompare(b.title||b.path)).forEach(d=>{
    const el=document.createElement('div');el.className='rail-item'+((current===d.path)?' on':'');
    el.innerHTML='<div class="t"></div><div class="p"></div>';
    el.querySelector('.t').textContent=d.title||d.path.split('/').pop();
    el.querySelector('.p').textContent=d.path;el.onclick=()=>openDoc(d.path);list.appendChild(el);
  });
  $('rail-count').textContent=r.docs.length;'''
new = '''async function loadRail(){
  const r=await api('/docs');
  renderTree(r.docs);
  $('rail-count').textContent=r.docs.length;'''
assert old in s
s = s.replace(old, new, 1)

old = 'async function refreshAll(){await Promise.all([loadGraph(),loadRail(),loadStatus(),loadMeta()]);}'
new = '''async function refreshAll(){await Promise.all([loadGraph(),loadRail(),loadStatus(),loadMeta()]);}

/* ───────── 侧栏树 ───────── */
function tagColor(t){let h=0;for(const c of t)h=(h*31+c.codePointAt(0))%360;const dark=document.documentElement.dataset.theme==='dark';return `oklch(${dark?'72%':'52%'} .14 ${h})`}
let treeState=JSON.parse(localStorage.getItem('litmus_tree')||'{}');
function renderTree(docs){
  const q=($('rail-search').value||'').trim().toLowerCase();
  const box=$('rail-list');box.innerHTML='';
  const root={dirs:new Map(),docs:[]};
  for(const d of docs){
    const parts=d.path.replace(/^Notes\\//,'').split('/');
    let node=root;
    for(let i=0;i<parts.length-1;i++){
      const seg=parts[i];
      if(!node.dirs.has(seg))node.dirs.set(seg,{dirs:new Map(),docs:[],path:parts.slice(0,i+1).join('/')});
      node=node.dirs.get(seg);
    }
    node.docs.push(d);
  }
  const match=d=>!q||(d.title||'').toLowerCase().includes(q)||d.path.toLowerCase().includes(q)||(d.tags||[]).some(t=>t.toLowerCase().includes(q));
  const hitIn=(node)=>{
    if(node.docs.some(match))return true;
    for(const c of node.dirs.values())if(hitIn(c))return true;
    return false;
  };
  function renderFolder(node,el,depth){
    for(const [name,child] of node.dirs){
      if(q&&!hitIn(child))continue;
      const open=q||treeState[child.path]!==false;
      const f=document.createElement('div');f.className='tree-folder'+(open?' open':'');
      const row=document.createElement('div');row.className='tree-row';row.style.marginLeft=depth*0+'px';
      row.innerHTML=`<svg class="chev" viewBox="0 0 24 24"><path d="m9 6 6 6-6 6"/></svg><span class="nm"></span><span class="ct">${child.docs.length}</span>`;
      row.querySelector('.nm').textContent=name;
      row.onclick=e=>{if(e.target.closest('.ct'))return;treeState[child.path]=!(treeState[child.path]!==false);localStorage.setItem('litmus_tree',JSON.stringify(treeState));f.classList.toggle('open');};
      row.ondragover=e=>{e.preventDefault();row.classList.add('drop-hot')};
      row.ondragleave=()=>row.classList.remove('drop-hot');
      row.ondrop=async e=>{e.preventDefault();row.classList.remove('drop-hot');
        const from=e.dataTransfer.getData('text/litmus-path');if(!from)return;
        const fname=from.split('/').pop();
        const to=`Notes/${child.path}/${fname}`;
        if(to===from)return;
        try{await api('/doc/move',{method:'POST',json:{from,to}});toast(`${from} → ${to}`,'ok');await refreshAll();if(current===from)openDoc(to);}
        catch(err){toast('移动失败：'+err.message,'err');}};
      f.appendChild(row);
      const kids=document.createElement('div');kids.className='tree-kids';
      renderFolder(child,kids,depth+1);
      f.appendChild(kids);
      el.appendChild(f);
    }
    for(const d of node.docs){
      if(q&&!match(d))continue;
      const leaf=document.createElement('div');leaf.className='tree-leaf'+(current===d.path?' on':'');
      leaf.draggable=true;
      const tags=(d.tags||[]).slice(0,3).map(t=>`<span class="tagchip" style="background:${tagColor(t)}">${t}</span>`).join('');
      leaf.innerHTML=`<span class="nm"></span>${tags}`;
      leaf.querySelector('.nm').textContent=d.title||d.path.split('/').pop();
      leaf.title=d.path;
      leaf.onclick=()=>openDoc(d.path);
      leaf.ondragstart=e=>{e.dataTransfer.setData('text/litmus-path',d.path);e.dataTransfer.effectAllowed='move'};
      el.appendChild(leaf);
    }
  }
  const wrap=document.createElement('div');wrap.className='tree';renderFolder(root,wrap,0);box.appendChild(wrap);
  updateDepth();
}
$('rail-search').addEventListener('input',()=>loadRail());'''
assert old in s
s = s.replace(old, new, 1)

# 6) /docs 返回值带 tags：graph 里已有 tags；rail 用 /docs——让它带 tags（改 server）
#    （在另一补丁里处理）

open(p, 'w', encoding='utf-8').write(s)
print('tree + depth done')
