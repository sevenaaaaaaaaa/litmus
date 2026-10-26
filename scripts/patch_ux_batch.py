import re

# ═══ 1) 登录门禁（隐私保护）═══
p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

login_js = '''
/* ───────── 登录门禁 ───────── */
function showLogin(){
  document.body.innerHTML='<div style="position:fixed;inset:0;z-index:999;display:grid;place-items:center;background:var(--bg)">'
    +'<div style="text-align:center;padding:40px;border-radius:26px;border:1px solid var(--border);background:var(--surface);backdrop-filter:blur(18px);box-shadow:var(--shadow)">'
    +'<div style="font-family:var(--font-display);font-size:22px;font-weight:700;margin-bottom:6px">Litmus Studio</div>'
    +'<div style="color:var(--faint);font-size:13px;margin-bottom:18px">输入访问令牌以继续</div>'
    +'<input id="login-token" placeholder="API Token" style="width:280px;padding:10px 14px;border-radius:12px;border:1px solid var(--border);background:var(--bg-soft);color:var(--fg);outline:none;font:14px var(--font-mono)">'
    +'<br><br><button onclick="doLogin()" style="padding:10px 28px;border-radius:12px;border:none;background:var(--accent);color:var(--on-accent);font:600 14px var(--font-body);cursor:pointer">进入</button>'
    +'</div></div>';
}
function doLogin(){
  const t=document.getElementById('login-token').value.trim();
  if(!t)return;
  localStorage.setItem('litmus_token',t);
  location.reload();
}
// 无 token → 登录页
if(!token){
  // token 可能在 URL ?token= 里但 localStorage 为空：上面已存。否则弹登录
  if(!localStorage.getItem('litmus_token')){
    showLogin();
    // 阻止后续初始化
    window.__login_gate=true;
  }
}
if(window.__login_gate){throw new Error('login gate')}
'''

old_init = "(async function init(){"
new_init = login_js + "\n(async function init(){"
assert old_init in s
s = s.replace(old_init, new_init, 1)

# ═══ 2) Canvas 触摸手势（pinch zoom + drag pan）═══
touch_js = '''
/* ───────── 触摸手势：捏合缩放 + 单指拖拽 ───────── */
let pinch=null;
canvas.addEventListener('touchstart',e=>{
  if(e.touches.length===2){
    const [a,b]=[...e.touches];
    pinch={d:Math.hypot(a.clientX-b.clientX,a.clientY-b.clientY),k:vp.k,
           cx:(a.clientX+b.clientX)/2,cy:(a.clientY+b.clientY)/2};
  }
},{passive:true});
canvas.addEventListener('touchmove',e=>{
  e.preventDefault();
  if(pinch&&e.touches.length===2){
    const [a,b]=[...e.touches];
    const nd=Math.hypot(a.clientX-b.clientX,a.clientY-b.clientY);
    const f=nd/pinch.d; const nk=Math.min(2.4,Math.max(.2,pinch.k*f));
    // 以捏合中心为锚
    const wx=(pinch.cx-vp.x)/vp.k, wy=(pinch.cy-vp.y)/vp.k;
    vp.k=nk; vp.x=pinch.cx-wx*nk; vp.y=pinch.cy-wy*nk;
    applyVP();queueDepth();
  }
},{passive:false});
canvas.addEventListener('touchend',e=>{if(e.touches.length<2)pinch=null});
'''

old_wheel = "canvas.addEventListener('wheel',e=>{"
assert old_wheel in s
s = s.replace(old_wheel, touch_js + "\n" + old_wheel, 1)

# ═══ 3) 抽屉：上传附件按钮（本地内容上传）+ PDF/DOCX 预览 ═══
s = s.replace('''    <button class="icon-btn" id="d-pub" title="发布">''','''    <button class="icon-btn" id="d-upload" title="上传附件（图片/PDF/DOCX）"><svg viewBox="0 0 24 24"><path d="M12 5v14M5 12l7-7 7 7"/></svg></button>
    <input type="file" id="d-file" style="display:none" multiple accept=".png,.jpg,.jpeg,.gif,.webp,.svg,.pdf,.docx,.doc,.md,.html,.txt,.csv,.json">
    <button class="icon-btn" id="d-pub" title="发布">''', 1)

upload_js = '''
/* ───────── 本地上传（图片/PDF/DOCX/HTML）───────── */
$('d-upload').onclick=()=>$('d-file').click();
$('d-file').addEventListener('change',async e=>{
  for(const f of [...e.target.files]){
    const buf=new Uint8Array(await f.arrayBuffer());
    const ext=f.name.split('.').pop().toLowerCase();
    if(['png','jpg','jpeg','gif','webp','svg'].includes(ext)){
      // 图片 → 附件入库 + Markdown 引用
      try{
        const r=await api('/asset?name='+encodeURIComponent(f.name),{method:'POST',body:buf,headers:{'content-type':'application/octet-stream'}});
        const t=$('editor'),s0=t.selectionStart;
        t.value=t.value.slice(0,s0)+r.markdown+'\\n'+t.value.slice(s0);dirty=true;
        toast(`图片入库 ${r.hash.slice(0,8)}…`,'ok');
      }catch(err){toast('图片上传失败：'+err.message,'err')}
    } else if(['pdf','docx','doc'].includes(ext)){
      // PDF/DOCX → 内容寻址存储 + 嵌入引用
      try{
        const r=await api('/asset?name='+encodeURIComponent(f.name),{method:'POST',body:buf,headers:{'content-type':'application/octet-stream'}});
        const embed=ext==='pdf'?`\\n[📄 ${f.name}](${r.path})\\n<embed src="${r.path}" type="application/pdf" width="100%" height="500px">\\n`
                  :`\\n[📝 ${f.name}](${r.path})\\n`;
        const t=$('editor'),s0=t.selectionStart;
        t.value=t.value.slice(0,s0)+embed+t.value.slice(s0);dirty=true;
        toast(`${ext.toUpperCase()} 已入库`,'ok');
      }catch(err){toast('上传失败：'+err.message,'err')}
    } else if(['md','html','txt','csv','json'].includes(ext)){
      // 文本文件 → 新文档
      try{
        const text=new TextDecoder().decode(buf);
        const slug=f.name.replace(/\\.[^.]+$/,'').toLowerCase().replace(/[^a-z0-9\\u4e00-\\u9fa5]+/g,'-').replace(/^-|-$/g,'')||('upload-'+Date.now());
        const path=`Notes/${slug}.md`;
        await api('/doc?path='+encodeURIComponent(path),{method:'PUT',body:text,headers:{'content-type':'text/markdown'}});
        toast(`已导入 ${f.name}`,'ok');
        await refreshAll();
      }catch(err){toast('导入失败：'+err.message,'err')}
    }
  }
  e.target.value='';
});
'''

old_dpub = "$('d-pub').onclick=()=>current?doPublish(current):toast('先打开一篇文档','err');"
assert old_dpub in s
s = s.replace(old_dpub, upload_js + "\n" + old_dpub, 1)

# ═══ 4) 网页本地化（fetch URL → MD → 文档）═══
s = s.replace('''  ['采集主题（快速建库）','t',async()=>{''','''  ['网页本地化','w',async()=>{
    closeAll();const url=prompt('网页 URL','https://example.com/article');if(!url)return;
    try{
      const r=await api('/ingest/web',{method:'POST',json:{url}});
      toast(`已本地化「${r.title||url}」→ ${r.rel}`,'ok');await refreshAll();openDoc(r.rel);
    }catch(e){toast('本地化失败：'+e.message,'err')}
  }],
  ['采集主题（快速建库）','t',async()=>{''', 1)

# 网络内容 embed（在文档中插入 iframe 引用）
s = s.replace('''const SLASH=[['表格','| a | b |\\n| --- | --- |\\n| 1 | 2 |'],['列表','- 甲\\n- 乙'],['引用','> 引用'],['代码','```rust\\n\\n```'],['分隔线','---'],['看板视图','> 视图：看板（本目录）'],['时间线视图','> 视图：时间线'],['锚点',' ^anchor-1'],['标签','#新标签 ']];''',
'''const SLASH=[['表格','| a | b |\\n| --- | --- |\\n| 1 | 2 |'],['列表','- 甲\\n- 乙'],['引用','> 引用'],['代码','```rust\\n\\n```'],['分隔线','---'],['锚点',' ^anchor-1'],['标签','#新标签 '],['嵌入网页','<iframe src="https://example.com" style="width:100%;height:400px;border:1px solid #ddd;border-radius:12px"></iframe>'],['嵌入 PDF','<embed src="path/to/file.pdf" type="application/pdf" width="100%" height="500px">']];''', 1)

# slash menu items 更新
s = s.replace('''  const items=[['插入表格','| a | b |\\n| --- | --- |\\n| 1 | 2 |'],['插入列表','- 甲\\n- 乙'],['插入引用','> 引用内容'],['插入代码块','```rust\\n\\n```'],['插入分隔线','---'],['插入锚点',' ^anchor-1'],['插入标签','#新标签 ']];''',
'''  const items=[['插入表格','| a | b |\\n| --- | --- |\\n| 1 | 2 |'],['插入列表','- 甲\\n- 乙'],['插入引用','> 引用内容'],['插入代码块','```rust\\n\\n```'],['插入分隔线','---'],['插入锚点',' ^anchor-1'],['插入标签','#新标签 '],['嵌入网页','<iframe src="https://" style="width:100%;height:400px;border:1px solid #ddd;border-radius:12px"></iframe>'],['嵌入 PDF','<embed src="Assets/…/file.pdf" type="application/pdf" width="100%" height="500px">']];''', 1)

open(p, 'w', encoding='utf-8').write(s)
print('client: login gate + touch + upload + web local + embed')
