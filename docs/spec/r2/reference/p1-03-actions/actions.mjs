// 五动作规格的校验器与生成器（P1-03）
// 规格是 JSON，界面文案/键位/埋点/失败行为都从它生成。
// 这里做的是把"规格里说不通"的地方变成错误，而不是靠人读文档。
import { readFileSync } from 'node:fs';

const MOD_ORDER = ['ctrl', 'opt', 'alt', 'shift', 'cmd', 'meta', 'mod'];
const MOD_ALIAS = { alt: 'opt', meta: 'cmd', mod: 'cmd' };
const ALLOWED_LANDING = ['new-card', 'existing-card', 'edge', 'doc', 'page', 'view'];

export function normalizeKey(raw) {
  if (typeof raw !== 'string' || !raw.trim()) return '';
  const parts = raw.toLowerCase().split('+').map((p) => p.trim()).filter(Boolean);
  if (!parts.length) return '';
  const mods = new Set();
  let base = '';
  for (const p of parts) {
    const m = MOD_ALIAS[p] ?? p;
    if (MOD_ORDER.includes(m) && m !== 'meta') mods.add(m);
    else base = p;
  }
  // mod 在不同平台展开成 cmd；为了能比较，把 mod 统一成 cmd
  if (parts.includes('mod')) { mods.delete('mod'); mods.add('cmd'); }
  if (!base) return '';
  const ordered = ['ctrl', 'opt', 'shift', 'cmd'].filter((m) => mods.has(m));
  return [...ordered, base].join('+');
}

export function validateSpec(spec, { events = null } = {}) {
  const errors = [];
  const warnings = [];
  const actions = spec.actions ?? [];
  const byId = new Map();
  if (actions.length !== 5) errors.push('五动作必须正好五个，现在是 ' + actions.length);
  const eventNames = events ? events.events.map((e) => e.name) : null;

  for (const a of actions) {
    if (!a.id) { errors.push('有动作没有 id'); continue; }
    if (byId.has(a.id)) errors.push('动作 id 重复：' + a.id);
    byId.set(a.id, a);

    for (const f of ['label', 'blurb', 'landing', 'rule']) {
      if (!a[f]) errors.push(a.id + ' 缺 ' + f);
    }
    if (!ALLOWED_LANDING.includes(a.landing)) errors.push(a.id + ' 的落点不合法：' + a.landing);

    const keys = a.keys ?? {};
    for (const p of spec.platforms ?? []) {
      if (!(p in keys)) errors.push(a.id + ' 缺平台键位：' + p);
    }
    const allKeys = Object.values(keys).flat().filter(Boolean);
    if (!allKeys.length) errors.push(a.id + ' 没有任何键位（键盘不可达）');

    const gestures = a.gestures ?? {};
    const allGestures = Object.values(gestures).flat().filter(Boolean);
    if (!allGestures.length) errors.push(a.id + ' 没有任何鼠标/触控方式');

    if (a.writes) {
      if (!a.undoable) errors.push(a.id + ' 会写盘却不可撤销');
      if (!a.failure || a.failure.preserveInput !== true) errors.push(a.id + ' 会写盘却没有失败保稿');
      if (a.confirm !== true && a.landing === 'page') errors.push(a.id + ' 发布类落点必须二次确认');
    }
    if (!a.failure) errors.push(a.id + ' 缺失败行为');
    else {
      if (!a.failure.message) errors.push(a.id + ' 的失败没有可读文案');
      if (a.failure.noSilent !== true) errors.push(a.id + ' 允许静默失败');
    }

    // 「两个输入方式等价」必须逐操作可查：键盘能做的，鼠标也得能做。
    if (!Array.isArray(a.ops) || !a.ops.length) errors.push(a.id + ' 没有列出可执行的操作（无法验证等价）');
    else {
      for (const op of a.ops) {
        if (!op.name) errors.push(a.id + ' 有操作没名字');
        if (op.keyboard !== true || op.pointer !== true) {
          errors.push(a.id + ' 的操作「' + op.name + '」不是双向可达（键盘 ' + op.keyboard + ' / 指针 ' + op.pointer + '）');
        }
      }
    }

    if (!Array.isArray(a.telemetry) || !a.telemetry.length) errors.push(a.id + ' 没有任何埋点');
    else if (eventNames) {
      for (const t of a.telemetry) {
        if (!eventNames.includes(t)) errors.push(a.id + ' 引用了不存在的事件：' + t);
      }
    }
  }

  // 键位冲突（按平台、按规范化后的键）
  const seen = new Map();
  for (const a of actions) {
    for (const p of spec.platforms ?? []) {
      for (const k of a.keys?.[p] ?? []) {
        const nk = normalizeKey(k);
        if (!nk) continue;
        const key = p + ':' + nk;
        if (seen.has(key)) errors.push('键位冲突 ' + nk + '（' + p + '）：' + seen.get(key) + ' 与 ' + a.id);
        else seen.set(key, a.id);
      }
    }
  }

  // 与保留键冲突
  for (const r of spec.reserved ?? []) {
    const rk = normalizeKey(r.key);
    for (const a of actions) {
      for (const p of spec.platforms ?? []) {
        for (const k of a.keys?.[p] ?? []) {
          if (normalizeKey(k) === rk) errors.push(a.id + ' 占用了保留键 ' + rk + '（' + r.why + '）');
        }
      }
    }
  }

  // 每个状态恰好一个主动作，且动作存在、intent 与埋点枚举一致
  const states = spec.states ?? [];
  const stateNames = ['seed', 'grounded', 'tension', 'settled', 'shipped'];
  for (const s of stateNames) {
    const hits = states.filter((x) => x.state === s);
    if (hits.length !== 1) errors.push('状态 ' + s + ' 的主动作个数是 ' + hits.length + '，必须是 1');
  }
  const intents = events?.events.find((e) => e.name === 'advance.opened')?.properties.intent.values ?? null;
  for (const s of states) {
    if (!stateNames.includes(s.state)) errors.push('未知状态：' + s.state);
    if (!byId.has(s.action)) errors.push('状态 ' + s.state + ' 指向不存在的动作：' + s.action);
    if (s.action === 'advance' && intents && !intents.includes(s.intent)) {
      errors.push('状态 ' + s.state + ' 的 intent 不在埋点枚举里：' + s.intent);
    }
    if (!s.why) warnings.push('状态 ' + s.state + ' 没写为什么它是主动作');
  }

  return { ok: errors.length === 0, errors, warnings };
}

export function renderSheet(spec) {
  const rows = spec.actions.map((a) => {
    const mac = (a.keys.mac ?? []).join(' / ') || '—';
    const ios = (a.gestures.ios ?? []).join('、') || '—';
    const web = (a.keys.web ?? []).join(' / ') || '—';
    const land = a.landing;
    const undo = a.undoable ? '可撤销' : '不可撤销';
    const parity = a.ops.filter((o) => o.keyboard && o.pointer).length + '/' + a.ops.length;
    return '| **' + a.label + '** | `' + mac + '` | ' + web + ' | ' + ios + ' | ' + land + ' | ' + undo + ' | ' + parity + ' | ' + a.failure.message + ' |';
  });
  const states = spec.states.map((s) => '| ' + s.state + ' | ' + s.action + (s.intent ? '（' + s.intent + '）' : '') + ' | ' + s.why + ' |');

  return [
    '# 五动作速查（由 actions.spec.json 生成，别手改）',
    '',
    '| 动作 | Mac 键 | Web 键 | iPhone | 落点 | 撤销 | 双向可达 | 失败时说什么 |',
    '|---|---|---|---|---|---|---|---|',
    ...rows,
    '',
    '## 每个状态的主动作',
    '',
    '| 状态 | 主动作 | 为什么 |',
    '|---|---|---|',
    ...states,
    '',
    '## 保留键（不许被动作占用）',
    '',
    ...spec.reserved.map((r) => '- `' + r.key + '`：' + r.why),
    '',
  ].join(String.fromCharCode(10));
}

export function loadSpec(url) {
  return JSON.parse(readFileSync(url, 'utf8'));
}