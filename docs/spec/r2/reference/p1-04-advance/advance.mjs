// 推进闭环（P1-04）：范围 → 意图 → 候选 → 来源 → 确认 → 落盘 → 连回 → 撤销
// 四条必须成立（都有测试）：
//   1) 取消 = 零写入（目录快照逐字节相同）
//   2) 候选 = 不写盘
//   3) 没来源 / 引文对不上 = 不许落盘
//   4) 撤销后目录逐字节回到落盘前
// 另外：同一件事重试不产生第二张卡（幂等键），连回写的是真实 wikilink。
import fs from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname } from 'node:path';
import { verifyClaim, squash } from '../p1-11-citations/citations.mjs';

const NL = String.fromCharCode(10);
const sha = (s) => createHash('sha256').update(String(s)).digest('hex');
const short = (s) => sha(s).slice(0, 24);

export const STATUS = ['open', 'candidate', 'verified', 'blocked', 'committed', 'cancelled', 'undone'];

// 幂等键：范围（锚点排序）+ 意图 + 意图原文 + 模式。重试同一次推进得到同一个 key；
// 换意图、换文字、换范围都必须得到不同的 key，否则会把两件事当成一件。
export function runKey({ space, anchors, intent, text, mode = 'model' }) {
  return short([space, [...anchors].sort().join(','), intent, squash(text), mode].join('|'));
}

// 落盘键：run + 候选正文。候选被改过就是另一张卡，不能顶替原来那张。
export function writeKeyOf(key, candidateText) {
  return short(key + '|' + squash(candidateText));
}

const advDir = (vault) => join(vault, '.litmus/local/advance');
const ledgerPath = (vault) => join(advDir(vault), 'ledger.jsonl');

export function makeRun(vault, { space, anchors, intent, text, mode = 'model' }) {
  const key = runKey({ space, anchors, intent, text, mode });
  return {
    vault, key, space, anchors: [...anchors], intent, text, mode,
    status: 'open', candidate: null, writeKey: null, verification: null,
  };
}

export function attachCandidate(run, { text, citations = [] }) {
  if (run.status === 'cancelled') throw new Error('run 已取消');
  if (run.status === 'committed') throw new Error('run 已落盘，不能再改候选');
  run.candidate = { text, citations };
  run.writeKey = writeKeyOf(run.key, text);
  run.status = 'candidate';
  return run.candidate;
}

// 闸门：推进的产物必须有来源。没来源 → 它不是推进结果，只是一张新的种子卡。
export function evaluate(run, resolve) {
  if (!run.candidate) throw new Error('还没有候选');
  const res = verifyClaim({ id: run.key, text: run.candidate.text, citations: run.candidate.citations }, resolve);
  run.verification = res;
  if (res.status === 'ungrounded') { run.status = 'blocked'; return { ...res, blockReason: '没有来源' }; }
  if (res.status === 'broken') { run.status = 'blocked'; return { ...res, blockReason: '来源对不上' }; }
  run.status = 'verified';
  return { ...res, blockReason: null, warn: res.status === 'weak' ? '只有转述：可以落盘，但对外发布时会被要求确认' : null };
}

const readLedger = (vault) => {
  const p = ledgerPath(vault);
  if (!fs.existsSync(p)) return [];
  return fs.readFileSync(p, 'utf8').split(NL).filter(Boolean).flatMap((l) => {
    try { return [JSON.parse(l)]; } catch { return []; }
  });
};

const appendLedger = (vault, entry) => {
  fs.mkdirSync(advDir(vault), { recursive: true });
  fs.appendFileSync(ledgerPath(vault), JSON.stringify(entry) + NL);
};

const safeSpace = (space) => String(space || 'Inbox').replaceAll('/', '-');

// 哪些落盘已经被撤销过（账本里一条 undo 记录算一次）
const undoneKeys = (vault) => new Set(readLedger(vault).filter((e) => e.type === 'undo').map((e) => e.writeKey));

export function cardPathFor(run) {
  return join(run.vault, 'Notes', safeSpace(run.space), 'advance-' + run.writeKey + '.md');
}

export function commit(run, resolve) {
  // 先看幂等：同一件事重试，直接返回上一张卡，不重复写。
  const undone = undoneKeys(run.vault);
  const existing = readLedger(run.vault).filter((e) => e.type === 'commit' && e.writeKey === run.writeKey && !undone.has(e.writeKey));
  if (existing.length) {
    run.status = 'committed';
    return { ok: true, duplicated: true, cardPath: existing[existing.length - 1].card, writeKey: run.writeKey };
  }
  if (run.status !== 'verified') throw new Error('未经核验不许落盘：' + run.status);

  const card = cardPathFor(run);
  if (fs.existsSync(card)) throw new Error('同名卡片已存在，拒绝覆盖：' + card);

  fs.mkdirSync(dirname(card), { recursive: true });
  const body = [
    '---',
    'type: thought',
    'source: advance',
    'intent: ' + run.intent,
    'run: ' + run.key,
    'state: ' + (run.verification?.status === 'weak' ? 'seed' : 'grounded'),
    'created: ' + new Date().toISOString(),
    '---',
    '',
    run.candidate.text,
    '',
    '## 来源',
    ...run.candidate.citations.map((c) => '- ^' + c.anchor + '（' + c.kind + '）'),
    '',
  ].join(NL);
  fs.writeFileSync(card, body);

  // 连回：在每一张来源卡末尾追加一行 wikilink；改之前先备份，撤销要逐字节还原。
  const undoDir = join(advDir(run.vault), 'undo', run.writeKey);
  fs.mkdirSync(undoDir, { recursive: true });
  const backups = [];
  const linked = [];
  run.candidate.citations.forEach((c, i) => {
    const src = resolve(c.anchor);
    if (!src) return;
    const abs = join(run.vault, src.path);
    if (!fs.existsSync(abs)) return;
    const backup = join(undoDir, i + '-' + src.path.replaceAll('/', '~'));
    fs.copyFileSync(abs, backup);
    backups.push({ path: src.path, backup });
    const line = '- 由 ^' + c.anchor + ' 推进 → [[' + 'advance-' + run.writeKey + ']]';
    const cur = fs.readFileSync(abs, 'utf8');
    if (!cur.includes(line)) fs.writeFileSync(abs, (cur.endsWith(NL) ? cur : cur + NL) + line + NL);
    linked.push(c.anchor);
  });

  appendLedger(run.vault, {
    type: 'commit', writeKey: run.writeKey, runKey: run.key, card, at: new Date().toISOString(),
    sources: run.candidate.citations.map((c) => c.anchor), linked, backups, status: run.verification?.status,
  });
  run.status = 'committed';
  return { ok: true, duplicated: false, cardPath: card, writeKey: run.writeKey, linked, backups: backups.length };
}

export function cancel(run) {
  if (run.status === 'committed') throw new Error('已经落盘，只能撤销不能取消');
  run.status = 'cancelled';
  return { ok: true };
}

export function undo(vault, writeKey) {
  const undone = undoneKeys(vault);
  if (undone.has(writeKey)) throw new Error('已经撤销过了：' + writeKey);
  const entries = readLedger(vault).filter((e) => e.type === 'commit' && e.writeKey === writeKey);
  if (!entries.length) throw new Error('没有这笔落盘记录：' + writeKey);
  const e = entries[entries.length - 1];

  if (fs.existsSync(e.card)) fs.rmSync(e.card);
  for (const b of e.backups ?? []) {
    const abs = join(vault, b.path);
    fs.mkdirSync(dirname(abs), { recursive: true });
    fs.copyFileSync(b.backup, abs);
  }
  // 备份目录也清掉：撤销完成后不该留下用户内容的副本。
  const undoDir = join(advDir(vault), 'undo', writeKey);
  if (fs.existsSync(undoDir)) fs.rmSync(undoDir, { recursive: true, force: true });
  appendLedger(vault, { type: 'undo', writeKey, at: new Date().toISOString() });
  return { ok: true, removed: e.card, restored: (e.backups ?? []).length };
}

// 目录快照：用来证明"零写入"和"逐字节还原"。
export function snapshot(dir) {
  const out = new Map();
  const walk = (d) => {
    if (!fs.existsSync(d)) return;
    for (const name of fs.readdirSync(d)) {
      const p = join(d, name);
      const st = fs.statSync(p);
      if (st.isDirectory()) walk(p);
      else out.set(p.slice(dir.length + 1), sha(fs.readFileSync(p)));
    }
  };
  walk(dir);
  return out;
}

export const diffSnapshots = (a, b) => {
  const added = [...b.keys()].filter((k) => !a.has(k));
  const removed = [...a.keys()].filter((k) => !b.has(k));
  const changed = [...b.keys()].filter((k) => a.has(k) && a.get(k) !== b.get(k));
  return { added, removed, changed, same: !added.length && !removed.length && !changed.length };
};
