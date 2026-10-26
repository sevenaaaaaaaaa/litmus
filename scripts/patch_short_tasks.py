import re

p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ═══════════ CSS ═══════════
css_anchor = '#toast{position:fixed;right:16px;bottom:44px;'
css_new = '''/* ── 分享面板 / 工作区面板 / 日历 ── */
#share-overlay{position:fixed;inset:0;z-index:210;display:none;place-items:center;background:color-mix(in oklab,var(--bg) 72%,transparent);backdrop-filter:blur(8px)}
#share-overlay.open{display:grid}
.share-card{width:min(480px,92vw);padding:24px;border-radius:var(--r-lg);background:var(--surface-strong);border:1px solid var(--border);box-shadow:var(--shadow)}
.share-card input{width:100%;padding:10px 14px;border-radius:12px;border:1px solid var(--border);background:var(--bg-soft);color:var(--fg);font:13px var(--font-mono);outline:none;margin:10px 0}
.share-card .link{font-family:var(--font-mono);font-size:11.5px;color:var(--accent);word-break:break-all;padding:10px;background:var(--hover);border-radius:10px;cursor:pointer}
.share-card .hint{font-size:11px;color:var(--faint);margin-top:8px}
#ws-panel{position:fixed;top:calc(var(--chrome-h)+20px);left:270px;right:290px;z-index:85;display:none;
  background:var(--surface-strong);border:1px solid var(--border);border-radius:var(--r-md);padding:14px;box-shadow:var(--shadow);backdrop-filter:blur(18px)}
#ws-panel.open{display:flex;gap:12px;flex-wrap:wrap;align-items:center}
.ws-chip{display:inline-flex;align-items:center;gap:7px;height:34px;padding:0 14px;border-radius:11px;border:1px solid var(--border);background:var(--surface);cursor:pointer;font:500 13px var(--font-body);transition:all .2s}
.ws-chip:hover{border-color:var(--border-strong);background:var(--surface-strong)}
.ws-chip.on{border-color:var(--accent);background:var(--accent-soft);color:var(--accent);font-weight:700}
#cal-ov{position:fixed;inset:0;z-index:190;display:none;background:var(--bg);overflow:auto;padding:60px 20px 20px}
#cal-ov.open{display:block}
.cal-head{display:flex;align-items:center;gap:14px;max-width:900px;margin:0 auto 16px}
.cal-head h2{font-family:var(--font-display);font-size:22px;margin:0;flex:1}
.cal-grid{display:grid;grid-template-columns:repeat(7,1fr);gap:3px;max-width:900px;margin:0 auto}
.cal-cell{min-height:80px;border:1px solid var(--border-soft);border-radius:8px;padding:4px;font-size:10px;font-family:var(--font-mono);color:var(--muted);background:var(--surface)}
.cal-cell.today{border-color:var(--accent);background:var(--accent-soft)}
.cal-cell .dn{font-weight:700;font-size:11px}
.cal-cell .dot-n{display:inline-block;width:5px;height:5px;border-radius:50%;margin:1px;background:var(--type-doc)}
.cal-cell .dot-n.cap{background:var(--type-capture)}
.cal-cell .dot-n.memo{background:var(--type-asset)}
.cal-empty{color:var(--faint);text-align:center;padding:60px;font-size:14px}
#toast{position:fixed;right:16px;bottom:44px;'''
assert css_anchor in s
s = s.replace(css_anchor, css_new, 1)

# ═══════════ HTML ═══════════
s = s.replace('</body>', '''<div id="share-overlay"><div class="share-card">
  <div style="font-family:var(--font-display);font-weight:700;font-size:17px;margin-bottom:4px">加密分享</div>
  <div style="color:var(--faint);font-size:12px;margin-bottom:10px">内容 AES-GCM 加密，密钥在链接 # 后面——服务器永远看不到</div>
  <input id="share-pass" placeholder="自定义密码（可选，增强安全）">
  <div class="link" id="share-link"></div>
  <div class="hint">点击链接复制 · 收件人打开即可查看（无需安装）</div>
  <br><button class="act" onclick="document.getElementById('share-overlay').classList.remove('open')" style="margin-top:10px">关闭</button>
</div></div>

<div id="ws-panel">
  <span class="kicker">工作区</span>
  <span id="ws-list"></span>
  <button class="ws-chip" id="ws-add" style="border-style:dashed">＋ 添加库</button>
  <button class="icon-btn" id="ws-close" style="width:26px;height:26px;margin-left:auto">✕</button>
</div>

<div id="cal-ov">
  <div class="cal-head">
    <button class="icon-btn" id="cal-prev"><svg viewBox="0 0 24 24" style="width:15px;height:15px;stroke:currentColor;stroke-width:1.8;fill:none"><path d="m15 18-6-6 6-6"/></svg></button>
    <h2 id="cal-title"></h2>
    <button class="icon-btn" id="cal-next"><svg viewBox="0 0 24 24" style="width:15px;height:15px;stroke:currentColor;stroke-width:1.8;fill:none"><path d="m9 18 6-6-6-6"/></svg></button>
    <button class="icon-btn" onclick="document.getElementById('cal-ov').classList.remove('open')">✕</button>
  </div>
  <div class="cal-grid" id="cal-grid"></div>
</div>
</body>''', 1)

# ═══════════ JS ═══════════
old_mode = '''/* ───────── 模式框架 ───────── */'''
new_mode = '''/* ───────── 加密分享 ───────── */
async function shareEncrypted(path){
  try{
    const d=await api('/doc?path='+encodeURIComponent(path));
    const pass=$('share-pass').value.trim()||crypto.getRandomValues(new Uint8Array(16)).reduce((a,b)=>a+b.toString(16).padStart(2,'0'),'');
    const enc=new TextEncoder().encode(JSON.stringify({title:d.title,markdown:d.markdown,html:d.html}));
    const key=await crypto.subtle.importKey('raw',new TextEncoder().encode(pass.padEnd(32,'0').slice(0,32)),{name:'AES-GCM'},false,['encrypt']);
    const iv=crypto.getRandomValues(new Uint8Array(12));
    const ct=await crypto.subtle.encrypt({name:'AES-GCM',iv},key,enc);
    const b64=btoa(String.fromCharCode(...new Uint8Array(ct)));
    const id=await api('/share',{method:'POST',json:{content:b64,iv:Array.from(iv)}}).then(r=>r.id);
    const url=`${location.origin}/s/${id}#k=${btoa(pass)}`;
    $('share-link').textContent=url;
    $('share-overlay').classList.add('open');
    $('share-link').onclick=()=>{navigator.clipboard?.writeText(url);toast('链接已复制','ok')};
  }catch(e){toast('分享失败：'+e.message,'err')}
}
$('share-pass')?.addEventListener('keydown',e=>{if(e.key==='Enter')shareEncrypted(current)});

/* ───────── 多库工作区 ───────── */
function loadWorkspaces(){return JSON.parse(localStorage.getItem('litmus_workspaces')||'[]')}
function renderWorkspaces(){
  const list=loadWorkspaces();const box=$('ws-list');box.innerHTML='';
  if(!list.find(w=>w.url===location.origin)){list.unshift({name:vaultName||'当前库',url:location.origin,token:token});localStorage.setItem('litmus_workspaces',JSON.stringify(list))}
  list.forEach((w,i)=>{
    const b=document.createElement('button');b.className='ws-chip'+(w.url===location.origin?' on':'');b.textContent=w.name;
    b.onclick=()=>{if(w.url!==location.origin){localStorage.setItem('litmus_token',w.token||token);location.href=w.url+'/?token='+w.token}};
    box.appendChild(b);
  });
}
$('ws-add').onclick=async()=>{
  const url=prompt('另一个 Litmus 的 URL','http://127.0.0.1:7700');if(!url)return;
  const t2=prompt('Token','');if(!t2)return;
  const name=prompt('显示名','新工作区')||url;
  const list=loadWorkspaces();list.push({name,url:url.replace(/\\/+$/,''),token:t2});
  localStorage.setItem('litmus_workspaces',JSON.stringify(list));renderWorkspaces();
};
$('ws-close').onclick=()=>$('ws-panel').classList.remove('open');
let vaultName='';
$('board-pill').oncontextmenu=e=>{e.preventDefault();$('ws-panel').classList.toggle('open');renderWorkspaces()};
$('board-pill').addEventListener('dblclick',()=>{$('ws-panel').classList.toggle('open');renderWorkspaces()});

/* ───────── 内容日历 ───────── */
let calY=new Date().getFullYear(),calM=new Date().getMonth();
async function openCalendar(){
  $('cal-ov').classList.add('open');renderCalendar();
}
async function renderCalendar(){
  const first=new Date(calY,calM,1);
  $('cal-title').textContent=`${calY} 年 ${calM+1} 月`;
  const days=new Date(calY,calM+1,0).getDate();
  const firstDow=first.getDay();
  const g=$('cal-grid');g.innerHTML='';
  for(let i=0;i<firstDow;i++)g.appendChild(document.createElement('div'));
  const docs=graph.nodes.filter(n=>n.kind!=='asset');
  const today=new Date();today.setHours(0,0,0,0);
  for(let d=1;d<=days;d++){
    const cell=document.createElement('div');cell.className='cal-cell';
    const dt=new Date(calY,calM,d);
    if(dt.getTime()===today.getTime())cell.classList.add('today');
    cell.innerHTML=`<div class="dn">${d}</div>`;
    docs.forEach(n=>{
      if(!n.mtime)return;
      const nd=new Date(n.mtime*1000);
      if(nd.getFullYear()===calY&&nd.getMonth()===calM&&nd.getDate()===d){
        const dot=document.createElement('span');dot.className='dot-n'+(n.kind==='capture'?' cap':'');
        dot.title=n.title||n.path;cell.appendChild(dot);
      }
    });
    cell.onclick=()=>{
      const dayDocs=docs.filter(n=>{if(!n.mtime)return false;const nd=new Date(n.mtime*1000);return nd.getFullYear()===calY&&nd.getMonth()===calM&&nd.getDate()===d});
      if(dayDocs.length===1)openDoc(dayDocs[0].path);
      else if(dayDocs.length>1){const names=dayDocs.map(n=>n.title||n.path.split('/').pop()).join('\\n');const pick=prompt(`该日 ${dayDocs.length} 篇：\\n${names}\\n\\n输入序号打开（1-${dayDocs.length}）：`);if(pick){const idx=parseInt(pick)-1;if(dayDocs[idx])openDoc(dayDocs[idx].path)}}
    };
    g.appendChild(cell);
  }
}
$('cal-prev').onclick=()=>{calM--;if(calM<0){calM=11;calY--}renderCalendar()};
$('cal-next').onclick=()=>{calM++;if(calM>11){calM=0;calY++}renderCalendar()};

/* ───────── 模式框架 ───────── */'''
assert old_mode in s
s = s.replace(old_mode, new_mode, 1)

# 命令面板
s = s.replace('''  ['采集主题（快速建库）','t',async()=>{''','''  ['内容日历','cal',async()=>{closeAll();await openCalendar()}],
  ['分享当前文档（加密）','sh',async()=>{if(current){$('share-pass').value='';shareEncrypted(current)}else toast('先打开一篇','err')}],
  ['工作区切换','ws',()=>{$('ws-panel').classList.toggle('open');renderWorkspaces()}],
  ['采集主题（快速建库）','t',async()=>{''', 1)

# vaultName from status
s = s.replace('''$('v-name').textContent=s.vault;''','''vaultName=s.vault;$('v-name').textContent=s.vault;''', 1)

open(p, 'w', encoding='utf-8').write(s)
print('share + workspaces + calendar done')
