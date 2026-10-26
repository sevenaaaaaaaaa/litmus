p = 'server/web/index.html'
s = open(p, encoding='utf-8').read()

# ── 主题预设：设计面板里加"主题预设"区块 ──
# 在设计面板 body 的导入区后加主题预设区块
old2 = '''    <input type="file" id="design-file" style="display:none" multiple>
    <div id="design-list"></div>'''
new2 = '''    <input type="file" id="design-file" style="display:none" multiple>
    <div class="dv-sec"><h4>主题预设（来自 OpenFlow）</h4><div id="preset-list" class="swrap" style="gap:8px"></div></div>
    <div id="design-list"></div>'''
assert old2 in s
s = s.replace(old2, new2, 1)

# JS：预设加载与应用（覆盖 app 的 CSS 变量，双主题）
old3 = "async function loadDesigns(){"
new3 = '''let themePreset=JSON.parse(localStorage.getItem('litmus_preset')||'null');
function applyThemePreset(){
  const el=document.getElementById('theme-override')||document.createElement('style');
  el.id='theme-override';
  if(!themePreset){el.textContent='';if(el.parentNode)el.remove();return}
  const t=document.documentElement.dataset.theme==='dark'?themePreset.dark:themePreset.light;
  const L=themePreset.layout||{};
  const map={'bg':'--bg','bg-soft':'--bg-soft','surface':'--surface','surface-strong':'--surface-strong','fg':'--fg','muted':'--muted','faint':'--faint','border':'--border','border-strong':'--border-strong','hover':'--hover','hover-strong':'--hover-strong','accent':'--accent','accent-strong':'--accent-strong','accent-soft':'--accent-soft','on-accent':'--on-accent','glass':'--glass','glass-bright':'--glass-bright','glass-border':'--glass-border','blob-a':'--blob-a','blob-b':'--blob-b','blob-c':'--blob-c','shadow':'--shadow','shadow-sm':'--shadow-sm'};
  let css=':root,\\n[data-theme="dark"]{';
  for(const [k,v] of Object.entries(t)){const our=map[k];if(our)css+=`${our}:${v};`}
  if(L['r-lg'])css+=`--r-lg:${L['r-lg']};--r-md:${L['r-md']||L['r-lg']};--r-sm:${L['r-sm']||L['r-md']||'12px'};`;
  css+='}';
  if(L['font-display'])css+=`\\nbody,#chrome,.brand{font-family:${L['font-display']}}`;
  if(L['font-mono'])css+=`\\n.n-path,.tagchip,.insp-path,#statusbar{font-family:${L['font-mono']}}`;
  el.textContent=css;
  document.head.appendChild(el);
  const b=$('preset-btn');if(b)b.textContent=themePreset?themePreset.name:'主题';
}
async function loadPresets(){
  const box=$('preset-list');if(!box)return;
  try{
    const d=await api('/design/presets');
    box.innerHTML='';
    for(const [k,v] of Object.entries(d.presets)){
      const b=document.createElement('button');
      b.className='act';b.style.cssText='height:auto;padding:7px 10px;flex-direction:column;align-items:flex-start;gap:3px';
      const a=v.light['accent']||'#';
      b.innerHTML=`<span style="font-weight:700">${v.name}</span><span style="display:flex;gap:4px"><i style="width:26px;height:12px;border-radius:4px;background:${v.light['bg']}"></i><i style="width:26px;height:12px;border-radius:4px;background:${a}"></i><i style="width:26px;height:12px;border-radius:4px;background:${v.dark['bg']}"></i></span>`;
      b.title=v.desc;
      const active=themePreset&&themePreset.name===v.name;
      if(active)b.style.borderColor='var(--accent)';
      b.onclick=async()=>{
        themePreset=active?null:v;
        localStorage.setItem('litmus_preset',JSON.stringify(themePreset));
        applyThemePreset();loadPresets();
        toast(active?'已恢复内置主题':'已应用预设：'+v.name,'ok');
      };
      box.appendChild(b);
    }
  }catch(e){}
}
async function loadDesigns(){'''
assert old3 in s
s = s.replace(old3, new3, 1)

# 设计面板打开时加载预设；主题切换时重应用
s = s.replace("async function openDesign(){await loadDesigns();openOverlay('design-ov');}",
              "async function openDesign(){await loadDesigns();await loadPresets();openOverlay('design-ov');}", 1)
s = s.replace('''$('theme').onclick=()=>{''', '''applyThemePreset();
const _oldSetTheme=$('theme').onclick;
$('theme').onclick=()=>{applyThemePreset();''', 1)
# 启动也应用
s = s.replace("await refreshAll();applyVP();fit();initPresence();", "applyThemePreset();await refreshAll();applyVP();fit();initPresence();", 1)

open(p, 'w', encoding='utf-8').write(s)
print('theme presets done')
