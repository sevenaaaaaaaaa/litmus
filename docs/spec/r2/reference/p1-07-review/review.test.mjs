import test from 'node:test';
import assert from 'node:assert/strict';
import { SECTIONS, scoreSegment, weightedKappa, judge, formatReport } from './review.mjs';

const IDS = SECTIONS.map((s) => s.id);
const all = (score) => IDS.map((id) => ({ id, a: score, b: score }));

// ── 单段 ──

test('分差 0–1：直接取均值，不触发裁决', () => {
  assert.deepEqual(scoreSegment(4, 4), {
    a: 4, b: 4, c: null, gap: 0, spread: null, score: 4, status: 'agreed',
  });
  const half = scoreSegment(4, 5);
  assert.equal(half.score, 4.5);
  assert.equal(half.status, 'agreed');
  assert.equal(half.gap, 1);
});

test('分差 2 且没有第三人：停在待裁决，不给分', () => {
  const r = scoreSegment(4, 2);
  assert.equal(r.status, 'needs-arbitration');
  assert.equal(r.score, null);
});

test('分差 2 + 第三人落在中间：取中位数裁决', () => {
  const r = scoreSegment(4, 2, 3);
  assert.equal(r.status, 'arbitrated');
  assert.equal(r.score, 3);
  assert.equal(r.spread, 2);
});

test('第三人把极差拉到 3 分以上：无法判定，回炉', () => {
  const r = scoreSegment(4, 2, 5);
  assert.equal(r.status, 'undecidable');
  assert.equal(r.score, null);
  assert.equal(r.spread, 3);
});

test('分差 ≥3：即使有第三人也不硬凑分数', () => {
  assert.equal(scoreSegment(5, 1, 3).status, 'undecidable');
});

test('分数越界 / 非整数 / 缺失：直接报错，不静默取整', () => {
  assert.throws(() => scoreSegment(0, 4), /1–5/);
  assert.throws(() => scoreSegment(6, 4), /1–5/);
  assert.throws(() => scoreSegment(4.5, 4), /1–5/);
  assert.throws(() => scoreSegment(4, 2, 3.2), /1–5/);
});

// ── κ ──

test('κ：完全一致为 1', () => {
  assert.equal(weightedKappa([{ a: 1, b: 1 }, { a: 5, b: 5 }, { a: 3, b: 3 }]), 1);
});

test('κ：完全相反为 -1', () => {
  const k = weightedKappa([{ a: 1, b: 5 }, { a: 5, b: 1 }]);
  assert.ok(Math.abs(k + 1) < 1e-12, String(k));
});

test('κ：允许相邻分歧，值落在 0 与 1 之间', () => {
  const k = weightedKappa([
    { a: 4, b: 4 }, { a: 4, b: 4 }, { a: 4, b: 4 }, { a: 4, b: 4 },
    { a: 4, b: 4 }, { a: 4, b: 4 }, { a: 4, b: 5 }, { a: 5, b: 4 },
    { a: 3, b: 4 }, { a: 4, b: 3 },
  ]);
  assert.ok(k >= 0 && k <= 1, String(k));
});

// ── 整卷 ──

test('10 段全 4 分：通过', () => {
  const r = judge(all(4));
  assert.equal(r.verdict, 'pass');
  assert.equal(r.good, 10);
  assert.equal(r.kappa, 1);
});

test('7 段 4 分 + 3 段 3 分：通过', () => {
  const ratings = all(3).map((r, i) => (i < 7 ? { ...r, a: 4, b: 4 } : r));
  const r = judge(ratings);
  assert.equal(r.good, 7);
  assert.equal(r.verdict, 'pass');
});

test('6 段 4 分：需要修改后重评', () => {
  const ratings = all(3).map((r, i) => (i < 6 ? { ...r, a: 4, b: 4 } : r));
  const r = judge(ratings);
  assert.equal(r.good, 6);
  assert.equal(r.verdict, 'revise');
});

test('3 段 4 分：不通过', () => {
  const ratings = all(2).map((r, i) => (i < 3 ? { ...r, a: 4, b: 4 } : r));
  const r = judge(ratings);
  assert.equal(r.good, 3);
  assert.equal(r.verdict, 'fail');
});

test('有段待裁决：整卷先停在 needs-arbitration，不给结论', () => {
  const ratings = all(4);
  ratings[0] = { id: ratings[0].id, a: 5, b: 2 };
  const r = judge(ratings);
  assert.equal(r.pending, 1);
  assert.equal(r.verdict, 'needs-arbitration');
});

test('有段无法判定：最多 revise，绝不 pass', () => {
  const ratings = all(4);
  ratings[0] = { id: ratings[0].id, a: 5, b: 1, c: 3 };
  const r = judge(ratings);
  assert.equal(r.undecidable, 1);
  assert.equal(r.verdict, 'revise');
});

test('无法判定的段超过 2 条：不通过', () => {
  const ratings = all(4);
  for (let i = 0; i < 3; i += 1) ratings[i] = { id: ratings[i].id, a: 5, b: 1, c: 3 };
  const r = judge(ratings);
  assert.equal(r.undecidable, 3);
  assert.equal(r.verdict, 'fail');
});

test('缺段 / 重复段 / 未知段：报错，不补默认分', () => {
  assert.throws(() => judge(all(4).slice(0, 9)), /9/);
  const dup = all(4);
  dup[1] = { ...dup[0] };
  assert.throws(() => judge(dup), /重复/);
  const unknown = all(4);
  unknown[0] = { id: 'nope', a: 4, b: 4 };
  assert.throws(() => judge(unknown), /未知段/);
});

test('报告包含每段、总分、κ 与判定', () => {
  const text = formatReport(judge(all(4)));
  for (const id of IDS) assert.ok(text.includes(id), id);
  assert.match(text, /达到 4 分：10\/10/);
  assert.match(text, /二次加权 κ：1\.000/);
  assert.match(text, /判定：pass/);
});
