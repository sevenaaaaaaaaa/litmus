import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  makeRun, attachCandidate, evaluate, commit, cancel, undo, snapshot, diffSnapshots, runKey, writeKeyOf, cardPathFor,
} from './advance.mjs';

const NL = String.fromCharCode(10);

function makeVault() {
  const dir = mkdtempSync(join(tmpdir(), 'litmus-adv-'));
  mkdirSync(join(dir, 'Notes/写作'), { recursive: true });
  mkdirSync(join(dir, '.litmus/local'), { recursive: true });
  writeFileSync(join(dir, 'Notes/写作/a.md'), '# A' + NL + NL + '判断不是结论，是被反复使用过的取舍。 ^a-1' + NL);
  writeFileSync(join(dir, 'Notes/写作/b.md'), '# B' + NL + NL + '证据链的价值在于能点回原文。 ^b-1' + NL);
  return dir;
}
const SOURCES = {
  'a-1': { path: 'Notes/写作/a.md', text: '判断不是结论，是被反复使用过的取舍。' },
  'b-1': { path: 'Notes/写作/b.md', text: '证据链的价值在于能点回原文。' },
};
const resolve = (a) => SOURCES[a] ?? null;
// 用户内容 = Notes/。账本与备份是库的记账，不属于"用户文件"，单独断言。
const notesSnap = (v) => snapshot(join(v, 'Notes'));
const ledgerFile = (v) => join(v, '.litmus/local/advance/ledger.jsonl');

const openRun = (vault, extra = {}) => makeRun(vault, {
  space: '写作', anchors: ['a-1', 'b-1'], intent: 'evidence', text: '给这个判断找证据', ...extra,
});
const cand = {
  text: '判断的价值在于它被反复使用。',
  citations: [{ anchor: 'a-1', kind: 'direct', quote: '是被反复使用过的取舍' }],
};

test('幂等键：稳定、与锚点顺序无关、换意图就换键', () => {
  const a = runKey({ space: '写作', anchors: ['a-1', 'b-1'], intent: 'evidence', text: '找证据' });
  const b = runKey({ space: '写作', anchors: ['b-1', 'a-1'], intent: 'evidence', text: '找 证据' });
  const c = runKey({ space: '写作', anchors: ['a-1', 'b-1'], intent: 'counter', text: '找证据' });
  assert.equal(a, b);
  assert.notEqual(a, c);
  assert.equal(a.length, 24);
});

test('落盘键随候选正文变化：改过就算另一张卡', () => {
  const k = runKey({ space: 's', anchors: ['a-1'], intent: 'draft', text: 'x' });
  assert.equal(writeKeyOf(k, '一句'), writeKeyOf(k, '一句'));
  assert.notEqual(writeKeyOf(k, '一句'), writeKeyOf(k, '两句'));
});

test('取消 = 零写入：Notes 不变，账本都不产生', () => {
  const v = makeVault();
  const before = notesSnap(v);
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  cancel(run);
  assert.equal(run.status, 'cancelled');
  assert.equal(diffSnapshots(before, notesSnap(v)).same, true);
  assert.equal(existsSync(ledgerFile(v)), false, '取消不该留下账本');
});

test('候选阶段不写盘', () => {
  const v = makeVault();
  const before = notesSnap(v);
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  assert.equal(diffSnapshots(before, notesSnap(v)).same, true);
  assert.equal(existsSync(ledgerFile(v)), false);
});

test('没有来源：闸门拦住，落盘直接抛错且目录不变', () => {
  const v = makeVault();
  const before = notesSnap(v);
  const run = openRun(v);
  attachCandidate(run, { text: '凭空一句', citations: [] });
  const res = evaluate(run, resolve);
  assert.equal(run.status, 'blocked');
  assert.equal(res.blockReason, '没有来源');
  assert.throws(() => commit(run, resolve), /未经核验/);
  assert.equal(diffSnapshots(before, notesSnap(v)).same, true);
});

test('引文对不上：同样拦住', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, { text: 'x', citations: [{ anchor: 'a-1', kind: 'direct', quote: '原文里没有这句' }] });
  const res = evaluate(run, resolve);
  assert.equal(run.status, 'blocked');
  assert.equal(res.blockReason, '来源对不上');
  assert.throws(() => commit(run, resolve));
});

test('只有转述：可以落盘，但带提醒，卡片状态记 seed', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, { text: '转述出来的一句', citations: [{ anchor: 'b-1', kind: 'paraphrase' }] });
  const res = evaluate(run, resolve);
  assert.equal(run.status, 'verified');
  assert.match(res.warn, /只有转述/);
  const out = commit(run, resolve);
  const md = readFileSync(out.cardPath, 'utf8');
  assert.match(md, /state: seed/);
});

test('落盘：写卡 + 连回 wikilink + 记账', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const out = commit(run, resolve);
  assert.equal(out.ok, true);
  assert.ok(existsSync(out.cardPath));
  const card = readFileSync(out.cardPath, 'utf8');
  assert.match(card, /判断的价值在于它被反复使用/);
  assert.match(card, /- \^a-1（direct）/);
  const src = readFileSync(join(v, 'Notes/写作/a.md'), 'utf8');
  assert.match(src, /由 \^a-1 推进 → \[\[advance-/);
  const ledger = readFileSync(join(v, '.litmus/local/advance/ledger.jsonl'), 'utf8');
  assert.match(ledger, /"type":"commit"/);
  assert.ok(ledger.includes(run.key));
});

test('幂等：同一件事重试不会产生第二张卡', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const first = commit(run, resolve);
  const again = commit(run, resolve);
  assert.equal(again.duplicated, true);
  assert.equal(again.cardPath, first.cardPath);
  const cards = readdirSync(join(v, 'Notes/写作')).filter((n) => n.startsWith('advance-'));
  assert.equal(cards.length, 1);
  const commits = readFileSync(join(v, '.litmus/local/advance/ledger.jsonl'), 'utf8')
    .split(NL).filter((l) => l.includes('"type":"commit"'));
  assert.equal(commits.length, 1);
});

test('撤销：用户文件逐字节回到落盘前，备份目录也清掉', () => {
  const v = makeVault();
  const before = notesSnap(v);
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const out = commit(run, resolve);
  assert.equal(diffSnapshots(before, notesSnap(v)).same, false);
  const r = undo(v, out.writeKey);
  assert.equal(r.restored, 1);   // 只引用了 a-1，所以只备份/还原了一个文件
  assert.deepEqual(diffSnapshots(before, notesSnap(v)), { added: [], removed: [], changed: [], same: true });
  assert.equal(existsSync(join(v, '.litmus/local/advance/undo', out.writeKey)), false, '备份该清掉');
});

test('撤销两次：第二次明确报错，不静默', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const out = commit(run, resolve);
  undo(v, out.writeKey);
  assert.throws(() => undo(v, out.writeKey), /已经撤销过/);
});

test('撤销之后可以重新落盘', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const out = commit(run, resolve);
  undo(v, out.writeKey);
  assert.equal(existsSync(out.cardPath), false);
  const run2 = openRun(v);
  attachCandidate(run2, cand);
  evaluate(run2, resolve);
  const out2 = commit(run2, resolve);
  assert.ok(existsSync(out2.cardPath));
});

test('同名卡片已存在：拒绝覆盖', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  evaluate(run, resolve);
  const card = cardPathFor(run);
  mkdirSync(join(card, '..'), { recursive: true });
  writeFileSync(card, '别动我');
  assert.throws(() => commit(run, resolve), /拒绝覆盖/);
  assert.equal(readFileSync(card, 'utf8'), '别动我');
});

test('未核验直接落盘会被拒', () => {
  const v = makeVault();
  const run = openRun(v);
  attachCandidate(run, cand);
  assert.throws(() => commit(run, resolve), /未经核验/);
});
