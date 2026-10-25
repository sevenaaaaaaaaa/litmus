import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const html = fs.readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const save = html.slice(html.indexOf('async function persistEditor(){'), html.indexOf("$('editor').addEventListener('input',()=>{", html.indexOf('async function persistEditor(){')));

function harness() {
  const elements = { editor: { value: 'first draft' }, 'save-status': { textContent: '' } };
  const calls = [];
  const context = vm.createContext({
    $: id => elements[id], clearTimeout,
    window: { __browseCache: new Map() }, toast: () => {},
    api: (url, options) => new Promise((resolve, reject) => calls.push({ url, options, resolve, reject }))
  });
  vm.runInContext("let current='Notes/a.md',dirty=true,currentFormat='markdown',liveTimer=null,saveFlight=null;" + save, context);
  return { elements, calls, run: code => vm.runInContext(code, context) };
}
const tick = () => new Promise(resolve => setImmediate(resolve));

function sourceBetween(start, end) {
  const from = html.indexOf(start), to = html.indexOf(end, from);
  assert.ok(from >= 0 && to > from, 'function boundaries must exist');
  return html.slice(from, to);
}
function canvasHarness() {
  return vm.createContext({ NODE_W: 300, ROW: 196, COL: 366 });
}

test('canvas is the default on every screen, document mode remains visible', () => {
  assert.match(html, /<body class="workbench" data-view="canvas"/);
  assert.match(html, /view='canvas'/);
  assert.match(html, /data-view="library" aria-pressed="false">文档/);
  assert.doesNotMatch(html, /if\(matchMedia\('\(max-width:767px\)'\)\.matches&&view==='canvas'\)/);
});

test('overview contains all cards even when a narrow screen needs a small scale', () => {
  const ctx=canvasHarness();
  vm.runInContext(sourceBetween('function fitScale(', '/* 窗口跨过断点'),ctx);
  for(const [w,h,cw,ch] of [[328,388,680,800],[1100,500,2200,1700],[328,388,300,136]]){
    const k=vm.runInContext(`fitScale(${w},${h},${cw},${ch})`,ctx);
    assert.ok(k>0&&k<=1);
    assert.ok(cw*k<=w+.001&&ch*k<=h+.001);
  }
});

test('retained capabilities have unique concrete controls in the new shell', () => {
  const markup=html.slice(0,html.indexOf('<script>',html.indexOf('<body')));
  for(const id of ['board-pill','nav-learn','dock-magic','dock-pinhub','dock-recent','dock-user','nav-calendar','rail-lens','selection-details','ins-close','nav-collect','d-tools','d-export','d-upload','d-style','d-expand','nav-memo']){
    assert.equal([...markup.matchAll(new RegExp(`id="${id}"`,'g'))].length,1,id+' must exist exactly once');
  }
  assert.doesNotMatch(html,/\.workbench\[data-view="canvas"\] #inspector\{display:none\}/);
  assert.doesNotMatch(html,/\.workbench\[data-view="canvas"\] #board-pill\{display:none\}/);
  assert.match(html,/\$\('selection-details'\)\.onclick=\(\)=>selectNode\(selected,true\)/);
});

test('mobile mode strip is not clipped by the chrome scroll container', () => {
  assert.match(html,/\.workbench #chrome\{[^}]*overflow:visible/);
  assert.match(html,/\.workbench #view-pill\{position:absolute;[^}]*bottom:auto/);
});

test('offline failures do not claim persistence and desktop fallback is attempted first', () => {
  const apiSource=sourceBetween('async function api(', 'let graph=');
  assert.doesNotMatch(apiSource,/改动都落在本机|联网后自动合流/);
  assert.match(apiSource,/尚未保存/);
  assert.ok(apiSource.indexOf('await bridgeApi')<apiSource.indexOf('navigator.onLine===false'));
});

test('thoughts use the current folder and preserve same-name documents', () => {
  const ctx = canvasHarness();
  vm.runInContext(sourceBetween('function thoughtDraft(', 'function freeThoughtPosition('), ctx);
  const result = vm.runInContext("thoughtDraft('一个问题\\n第一条线索','Notes/主题',null,new Set(['Notes/主题/一个问题.md']))", ctx);
  assert.equal(result.path, 'Notes/主题/一个问题 (1).md');
  assert.match(result.markdown, /第一条线索/);
});

test('branches preserve an unambiguous source link in ordinary Markdown', () => {
  const ctx = canvasHarness();
  vm.runInContext(sourceBetween('function thoughtDraft(', 'function freeThoughtPosition('), ctx);
  const result = vm.runInContext("thoughtDraft('下一步','Notes',{path:'Notes/主题/来源.md'},new Set())", ctx);
  assert.match(result.markdown, /\[\[Notes\/主题\/来源.md\]\]/);
});

test('card placement avoids existing cards without moving them', () => {
  const ctx = canvasHarness();
  vm.runInContext(sourceBetween('function freeThoughtPosition(', 'async function submitThought('), ctx);
  const result = vm.runInContext("freeThoughtPosition({x:0,y:0},{a:{x:0,y:0},b:{x:0,y:196}})", ctx);
  assert.equal(result.x, 0);
  assert.equal(result.y, 392);
});

test('wiki edges resolve explicit paths, not just matching titles', () => {
  const ctx = canvasHarness();
  vm.runInContext(sourceBetween('function wikiEdges(', 'function relTime('), ctx);
  const edges = vm.runInContext("wikiEdges([{id:'a',path:'Notes/主题/来源.md',title:'来源'},{id:'b',path:'Notes/下一步.md',links:['Notes/主题/来源.md']}])", ctx);
  assert.equal(edges.length, 1);
  assert.equal(edges[0].from, 'b');
  assert.equal(edges[0].to, 'a');
});

test('board writes capture their board and serialize rapid position changes', async () => {
  const calls = [];
  const ctx = vm.createContext({api:(url,options)=>new Promise(resolve=>calls.push({url,options,resolve})),toast:()=>{}});
  vm.runInContext("let boardName='main',board={nodes:{a:{x:1,y:2}}},graph={edges:[]},boardSaveFlight=Promise.resolve();" + sourceBetween('function saveBoard(){', 'async function ensureCanvasBoard('), ctx);
  const first = vm.runInContext('saveBoard()', ctx);
  vm.runInContext("board.nodes.a.x=99;boardName='second';", ctx);
  const second = vm.runInContext('saveBoard()', ctx);
  await tick();
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/board?name=main');
  assert.equal(calls[0].options.json.nodes.a.x, 1);
  calls[0].resolve({});
  await first; await tick();
  assert.equal(calls[1].url, '/board?name=second');
  assert.equal(calls[1].options.json.nodes.a.x, 99);
  calls[1].resolve({});
  assert.equal(await second, true);
});

test('failed thought submission keeps the draft and allows retry', async () => {
  const elements = {
    'thought-input': {value:'保留这个想法',disabled:false},
    'thought-submit': {disabled:false}, 'thought-status': {textContent:''}
  };
  const ctx=vm.createContext({$:id=>elements[id],libraryPaths:async()=>new Set(),
    api:async()=>{throw new Error('offline');},toast:()=>{},s2w:()=>({x:0,y:0}),innerWidth:1000,innerHeight:800});
  vm.runInContext("let thoughtBusy=false,thoughtSource=null,thoughtPoint=null,browsePath='Notes',boardName='main',board={nodes:{}},vp={k:1};const NODE_W=300;"+
    sourceBetween('function thoughtDraft(', 'function freeThoughtPosition(')+
    sourceBetween('async function submitThought(', 'async function connectThought('),ctx);
  await vm.runInContext('submitThought({preventDefault(){}})',ctx);
  assert.equal(elements['thought-input'].value,'保留这个想法');
  assert.equal(elements['thought-input'].disabled,false);
  assert.equal(elements['thought-submit'].disabled,false);
  assert.match(elements['thought-status'].textContent,/保存失败/);
  assert.equal(vm.runInContext('thoughtBusy',ctx),false);
});

test('rapid typing is saved in order, including edits made during a request', async () => {
  const h = harness();
  const saved = h.run('persistEditor()');
  h.elements.editor.value = 'second draft';
  h.calls[0].resolve({});
  await tick();
  assert.equal(h.calls.length, 2);
  assert.equal(h.calls[0].options.body, 'first draft');
  assert.equal(h.calls[1].options.body, 'second draft');
  assert.equal(h.run('dirty'), true);
  h.calls[1].resolve({});
  assert.equal(await saved, true);
  assert.equal(h.run('dirty'), false);
  assert.equal(h.elements['save-status'].textContent, '已保存');
});

test('an old save cannot mark another document saved', async () => {
  const h = harness();
  const saved = h.run('persistEditor()');
  h.run("current='Notes/b.md'; dirty=true;");
  h.elements.editor.value = 'another document';
  h.calls[0].resolve({});
  await saved;
  assert.equal(h.run('dirty'), true);
  assert.equal(h.calls.length, 1);
  assert.match(h.calls[0].url, /Notes%2Fa.md/);
  assert.equal(h.elements.editor.value, 'another document');
});

test('failed saves preserve content and can be retried', async () => {
  const h = harness();
  const first = h.run('persistEditor()');
  h.calls[0].reject(new Error('offline'));
  assert.equal(await first, false);
  assert.equal(h.run('dirty'), true);
  assert.equal(h.elements.editor.value, 'first draft');
  assert.match(h.elements['save-status'].textContent, /保存失败/);
  const retry = h.run('persistEditor()');
  h.calls[1].resolve({});
  assert.equal(await retry, true);
  assert.equal(h.run('dirty'), false);
});

test('blur and navigation share a pending save', async () => {
  const h = harness();
  const blur = h.run('persistEditor()');
  const navigation = h.run('persistEditor()');
  assert.equal(h.calls.length, 1);
  h.calls[0].resolve({});
  assert.deepEqual(await Promise.all([blur, navigation]), [true, true]);
  assert.equal(h.calls.length, 1);
});

function dockHarness() {
  const classes=new Set(), storage=new Map();
  const element=()=>({value:'',hidden:false,attributes:{},classList:{remove(){},add(){},contains(){return false;}},setAttribute(k,v){this.attributes[k]=v;}});
  const els=Object.fromEntries(['say','thought-input','dock-search','dock-mode','send','palette-ov'].map(id=>[id,element()]));
  const ctx=vm.createContext({$:id=>els[id],localStorage:{setItem:(k,v)=>storage.set(k,v)},document:{body:{classList:{toggle(k,on){on?classes.add(k):classes.delete(k)},remove(k){classes.delete(k)}}}}});
  vm.runInContext("let dockMode='search';const dockDrafts={search:'',chat:''};let dockReturnMode='search';"+sourceBetween('function setDockMode(m){',"$('dock-mode').onclick="),ctx);
  return {els,classes,storage,run:code=>vm.runInContext(code,ctx)};
}

test('Dock modes keep independent drafts and do not silently request a model',()=>{
  const h=dockHarness();h.els.say.value='search draft';h.run("setDockMode('chat')");
  assert.equal(h.els.say.value,'');h.els.say.value='agent draft';h.run("setDockMode('search')");
  assert.equal(h.els.say.value,'search draft');h.run("setDockMode('chat')");
  assert.equal(h.els.say.value,'agent draft');
  assert.equal(h.els['dock-mode'].attributes['aria-pressed'],'true');
  h.run("setDockMode('invalid')");assert.equal(h.run('dockMode'),'chat');
});

test('temporary recording retains its draft and remembers its return mode',()=>{
  const h=dockHarness();h.run("setDockMode('chat')");h.els.say.value='agent draft';
  h.els['thought-input'].value='unfinished thought';h.run("setDockMode('record')");
  assert.equal(h.run('dockReturnMode'),'chat');assert.equal(h.els.say.hidden,true);
  assert.equal(h.els['thought-input'].hidden,false);assert.equal(h.storage.get('thirdc_dockmode'),'chat');
  h.run('setDockMode(dockReturnMode)');assert.equal(h.els.say.value,'agent draft');
  assert.equal(h.els['thought-input'].value,'unfinished thought');
});

test('Dock search submission never clears the query or invokes chat',()=>{
  const calls=[];const input={value:'reading'};
  const ctx=vm.createContext({$:()=>input,openPalette:q=>calls.push(['search',q]),chat:()=>calls.push(['chat'])});
  vm.runInContext("let dockMode='search';"+sourceBetween('function dockSubmit(){','/* 固定入口：默认'),ctx);
  vm.runInContext('dockSubmit()',ctx);assert.deepEqual(calls,[['search','reading']]);assert.equal(input.value,'reading');
});

test('Magic actions are not gated by the existence of the Dock trigger',()=>{
  const source=sourceBetween('const MF_ITEMS=', '/* ── 左簇：Studio');
  assert.doesNotMatch(source,/\$\('dock-magic'\)&&\(\(\)=>/);
  assert.match(source,/MF_ITEMS\.forEach\(el=>el\.addEventListener\('click'/);
  assert.match(source,/k==='grow'/);assert.match(source,/k==='action'/);
});

test('hidden shell retains visible recovery controls and does not override pinned preference at boot',()=>{
  assert.match(html,/setRailClosed\(pref!=='open',true,false\)/);
  assert.doesNotMatch(html,/setRailClosed\(view==='canvas'\|\|pref/);
  const markup=html.slice(0,html.indexOf('<script>',html.indexOf('<body')));
  for(const id of ['chrome-reveal','chrome-pin','chrome-hide','rail-pin','dock-reveal','dock-record','dock-layout','thought-close']){
    assert.equal([...markup.matchAll(new RegExp(`id="${id}"`,'g'))].length,1,id);
  }
  assert.doesNotMatch(markup,/id="assistant-toggle"/);
  assert.equal([...html.matchAll(/function dockSubmit\(/g)].length,1);
});

test('Escape dismisses Magic before resetting a recording source',()=>{
  const keyHandler=sourceBetween("document.addEventListener('keydown',e=>{", "const tag=(e.target.tagName||'').toLowerCase();");
  assert.ok(keyHandler.indexOf("$('magic-fan').classList.contains('open')")>=0);
  assert.ok(keyHandler.indexOf("$('magic-fan').classList.contains('open')")<keyHandler.indexOf('resetThoughtContext()'));
});
