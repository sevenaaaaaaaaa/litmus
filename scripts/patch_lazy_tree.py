import re

p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# 找到 renderTree 整个函数并替换为懒加载版本
start = s.index('function renderTree(docs){')
# 结束位置：下一个顶层函数（'/* ───────── 协作光标' 之前的 `}`）
end_marker = s.index("$('rail-search').addEventListener('input',()=>loadRail());", start)
end = s.index('\n', end_marker) + 1

new_fn = '''function renderTree(docs){
  const q=($('rail-search').value||'').trim().toLowerCase();
  const box=$('rail-list');box.innerHTML='';
  const root={dirs:new Map(),docs:[],path:''};
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
  const MAX_PER_DIR=200;
  // 渲染"某一层"（懒加载：只在该层展开时才渲染）
  function renderLevel(node,el){
    for(const [name,child] of node.dirs){
      if(q&&!hitIn(child))continue;
      const f=document.createElement('div');f.className='tree-folder';
      const row=document.createElement('div');row.className='tree-row';
      const childCount=countDocs(child);
      row.innerHTML=`<svg class="chev" viewBox="0 0 24 24"><path d="m9 6 6 6-6 6"/></svg><span class="nm"></span><span class="ct">${childCount}</span>`;
      row.querySelector('.nm').textContent=name;
      row.onclick=()=>{
        const willOpen=!f.classList.contains('open');
        f.classList.toggle('open',willOpen);
        if(willOpen&&!f.dataset.loaded){
          const kids=document.createElement('div');kids.className='tree-kids';
          renderLevel(child,kids);
          renderDocs(child,kids);
          f.appendChild(kids);f.dataset.loaded='1';
        }
      };
      row.ondragover=e=>{e.preventDefault();row.classList.add('drop-hot')};
      row.ondragleave=()=>row.classList.remove('drop-hot');
      row.ondrop=async e=>{
        e.preventDefault();row.classList.remove('drop-hot');
        const from=e.dataTransfer.getData('text/litmus-path');if(!from)return;
        const fname=from.split('/').pop();
        const to=`Notes/${child.path}/${fname}`;
        if(to===from)return;
        try{await api('/doc/move',{method:'POST',json:{from,to}});toast(`${from} → ${to}`,'ok');await refreshAll();if(current===from)openDoc(to);}
        catch(err){toast('移动失败：'+err.message,'err');}
      };
      f.appendChild(row);
      el.appendChild(f);
      if(q){ // 搜索时直接展开（命中路径）
        const kids=document.createElement('div');kids.className='tree-kids';
        renderLevel(child,kids);renderDocs(child,kids);
        f.appendChild(kids);f.dataset.loaded='1';f.classList.add('open');
      }
    }
    renderDocs(node,el);
  }
  function renderDocs(node,el){
    const list=q?node.docs.filter(match):node.docs;
    list.slice(0,MAX_PER_DIR).forEach(d=>el.appendChild(leafFor(d)));
    if(list.length>MAX_PER_DIR){
      const more=document.createElement('div');more.className='tree-row';
      more.innerHTML=`<span class="nm" style="color:var(--faint)">还有 ${list.length-MAX_PER_DIR} 篇…（用检索更快）</span>`;
      el.appendChild(more);
    }
  }
  function leafFor(d){
    const leaf=document.createElement('div');leaf.className='tree-leaf'+(current===d.path?' on':'');
    leaf.draggable=true;leaf.title=d.path;
    const tags=(d.tags||[]).slice(0,3).map(t=>`<span class="tagchip" style="background:${tagColor(t)}">${t}</span>`).join('');
    leaf.innerHTML=`<span class="nm"></span>${tags}`;
    leaf.querySelector('.nm').textContent=d.title||d.path.split('/').pop();
    leaf.onclick=()=>openDoc(d.path);
    leaf.ondragstart=e=>{e.dataTransfer.setData('text/litmus-path',d.path);e.dataTransfer.effectAllowed='move'};
    return leaf;
  }
  function countDocs(node){
    let n=node.docs.length;
    for(const c of node.dirs.values())n+=countDocs(c);
    return n;
  }
  renderLevel(root,box);
  updateDepth();
}
'''
s = s[:start] + new_fn + s[end:]

# 移除"整体入场动画"（大库时所有元素从 opacity:0 开始 → 看起来全空）
s = s.replace('.tree-leaf,.tree-folder{animation:treeIn .35s var(--ease-spring) both}', '')
# 缩进视觉：层级靠 tree-kids 的左边框，不再需要 depth 参数

open(p, 'w', encoding='utf-8').write(s)
print('lazy tree implemented')
