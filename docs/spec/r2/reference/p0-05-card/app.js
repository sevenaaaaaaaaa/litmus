import { STATES, ORDER, reduce, activeAction, coverage, quickCheck } from './card-state.mjs';

const CARDS = [
  {
    id: 'c1', title: '灵魂卡不是人设，是判断的压缩', live: true,
    gist: '它不该描述「我是谁」，而该给我一条能被反驳的标准：先看反例，能否一句话说清，能否落到一个动作。',
    state: 'tension', where: '灵魂卡',
    counts: { source: 3, counter: 1, related: 2 },
    tension: '与《记忆即产品》冲突：它主张记忆越多越像你',
    sources: [
      ['《How to Write a Good PRD》§1 问题不写方案', 'p.3 · 抓取 11-20'],
      ['与 A 的访谈 42:10「我不想再填一张表」', '转述 · 11-18'],
      ['自己的旧文《判断比信息贵》', '直接引用 · 直接'],
    ],
    history: ['11-21 由种子变有据（+1 来源）', '11-22 与《记忆即产品》冲突 → 有张力'],
    soulRefs: ['criteria-1 先看反例', 'refuse-3 不写没有来源的结论'],
  },
  {
    id: 'c2', title: '3 秒落下，比分类正确更重要', live: true,
    gist: '捕捉时只做保存。分诊、命名、连接都是之后的事；任何在 3 秒内要求用户做决定的界面都在偷走想法。',
    state: 'grounded', where: '快捕',
    counts: { source: 3, counter: 0, related: 4 },
    tension: null,
    sources: [
      ['flomo 的「只记录」假设', 'p.1 · 抓取 11-19'],
      ['Obsidian 快速捕获插件的失败路径', '转述 · 11-19'],
      ['自己的 37 次快捕日志', '直接引用 · 自己'],
    ],
    history: ['11-20 落下', '11-21 补 3 条来源 → 有据'],
    soulRefs: ['belief-1 知识是被用过的判断', 'voice-4 短句、先结论'],
  },
  {
    id: 'c3', title: '先有画布，再有文档', live: false,
    gist: '画布是思考的空间，文档是它的一个镜头；把文档做成另一套树，等于让用户在两个世界里各存一半自己。',
    state: 'seed', where: '画布',
    counts: { source: 0, counter: 0, related: 1 },
    tension: null,
    sources: [],
    history: ['刚刚落下'],
    soulRefs: [],
  },
];

let selected = CARDS[0].id;
let flipped = new Set();

const stackEl = document.getElementById('stack');
const toastEl = document.getElementById('toast');
const checkEl = document.getElementById('checkPanel');

const toast = (msg) => {
  toastEl.textContent = msg;
  toastEl.classList.add('on');
  clearTimeout(toast._t);
  toast._t = setTimeout(() => toastEl.classList.remove('on'), 2200);
};

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

function chip(label, n, bad) {
  return `<span class="chip${bad ? ' bad' : ''}">${esc(label)} ${n}</span>`;
}

function render() {
  stackEl.innerHTML = CARDS.map((c) => {
    const s = STATES[c.state];
    const isSel = c.id === selected;
    const back = flipped.has(c.id);
    const edge = c.state === 'seed' ? 2 : Math.round(s.coverage * 100);
    return `
<article class="card" tabindex="0" data-id="${c.id}" data-selected="${isSel}"
  style="--tone:var(--${s.tone});--tone-ink:var(--${s.tone}-ink)">
  <div class="edge" title="显色边：证据覆盖度 ${edge}%"><i style="height:${edge}%"></i></div>
  ${back ? `
  <div class="back">
    <h4>来源</h4>
    <ul>${c.sources.length ? c.sources.map(([t, m]) => `<li><a href="#" onclick="return false">${esc(t)}</a> · ${esc(m)}</li>`).join('') : '<li>没有找到来源</li>'}</ul>
    <h4>关系</h4>
    <ul><li>入链 ${c.counts.related} · 出链 1 · 同线索「显色」</li></ul>
    <h4>历史</h4>
    <ul>${c.history.map((h) => `<li>${esc(h)}</li>`).join('')}</ul>
    <h4>读了灵魂卡的哪几条</h4>
    <ul>${c.soulRefs.length ? c.soulRefs.map((r) => `<li>^${esc(r)}</li>`).join('') : '<li>这次没有读（未确认卡）</li>'}</ul>
    <div class="acts">
      <button class="primary" data-act="advance">${esc(s.action)}</button>
      <button data-act="link">连到</button>
      <button data-act="publish">发布</button>
      <button data-act="flip">翻回正面</button>
    </div>
  </div>` : `
  <div class="front">
    <h3 class="title">${esc(c.title)}</h3>
    <p class="gist">${esc(c.gist)}</p>
    <div class="chips">
      ${chip('来源', c.counts.source, false)}
      ${chip('反例', c.counts.counter, c.counts.counter > 0)}
      ${chip('相关', c.counts.related, false)}
    </div>
    <div class="state-row">
      <svg aria-hidden="true"><use href="#i-${s.icon}"></use></svg>
      <span>${esc(s.label)} · ${esc(s.meaning)}</span>
    </div>
    ${c.tension ? `<div class="next">⚑ 有张力：${esc(c.tension)}</div>` : ''}
    <div class="next">→ 下一步：<b>${esc(s.action)}</b></div>
    <div class="meta"><span>${esc(s.label)} · ${esc(c.where)}</span><span>${c.live ? '刚刚' : '2 分钟前'} · 你</span></div>
  </div>`}
</article>`;
  }).join('');

  for (const el of stackEl.querySelectorAll('.card')) {
    el.addEventListener('click', (e) => {
      const act = e.target.closest('[data-act]')?.dataset.act;
      const id = el.dataset.id;
      selected = id;
      if (act === 'flip') { flipped.delete(id); render(); return; }
      if (act === 'advance') { doAdvance(id); return; }
      if (act === 'link') { toast('连到：选另一张卡（原型只演示落点）'); return; }
      if (act === 'publish') { doPublish(id); return; }
      if (!flipped.has(id) && id === selected) { /* 单击选中 */ }
      render();
    });
  }
}

function cardOf(id) { return CARDS.find((c) => c.id === id); }

function doAdvance(id) {
  const c = cardOf(id);
  const r = reduce(c.state, c.state === 'seed' ? 'source.added' : c.state === 'grounded' ? 'source.conflict' : 'decide');
  if (!r.ok) { toast('不能推进：' + r.reason); return; }
  c.state = r.state;
  toast('候选卡（未写盘）· 按 ' + STATES[r.state].action + ' 继续');
  render();
}

function doPublish(id) {
  const c = cardOf(id);
  const r = reduce(c.state, 'publish');
  if (!r.ok) { toast('不能发布：' + r.reason); return; }
  c.state = r.state;
  toast('已发布为页面（原型不真的发布）');
  render();
}

function doFlip(id) {
  if (flipped.has(id)) flipped.delete(id); else flipped.add(id);
  selected = id;
  render();
}

document.addEventListener('keydown', (e) => {
  const sel = cardOf(selected);
  if (e.key === ' ' && !e.metaKey) { e.preventDefault(); doFlip(selected); return; }
  if (e.key === 'Escape') { flipped.clear(); checkEl.hidden = true; render(); return; }
  if (e.metaKey && e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); doAdvance(selected); return; }
  if (e.metaKey && e.shiftKey && e.key === 'Enter') { e.preventDefault(); toast('连到：⌘⇧↩ 已按下（原型不落库）'); return; }
  if (e.metaKey && e.shiftKey && (e.key === 'p' || e.key === 'P')) { e.preventDefault(); doPublish(selected); return; }
  if (sel && /^[1-5]$/.test(e.key)) {
    const st = ORDER[Number(e.key) - 1];
    sel.state = st; render();
  }
});

for (const b of document.querySelectorAll('.states button')) {
  b.addEventListener('click', () => {
    const card = cardOf(selected);
    card.state = b.dataset.state;
    render();
  });
}

document.getElementById('colorOnly').addEventListener('change', (e) => {
  document.body.dataset.a11y = e.target.checked ? 'color-only' : 'normal';
});

document.getElementById('check').addEventListener('click', () => {
  const s = STATES[cardOf(selected).state];
  const q = quickCheck(cardOf(selected).state, { space: cardOf(selected).where });
  const colorOnly = document.body.dataset.a11y === 'color-only';
  checkEl.hidden = false;
  checkEl.innerHTML = `
    <h4>3 秒自检（截图能不能答出来）</h4>
    <dl>
      <dt>我在哪</dt><dd>${esc(q.where)}</dd>
      <dt>什么状态</dt><dd>${esc(q.state)}</dd>
      <dt>下一步</dt><dd>${esc(q.next)}</dd>
    </dl>
    ${colorOnly ? '<div class="fail">当前是「只靠颜色」模式：文字与图标被藏起来了，三条都答不出——这就是为什么状态必须有文字与图标。</div>' : '<div class="fail" style="color:var(--ink-3)">当前：文字 + 图标 + 颜色三信道，三条都能答出。</div>'}`;
});

document.addEventListener('click', (e) => { if (e.target.id === 'check') return; });
render();

// 供测试与后续自动化引用
export { CARDS };
