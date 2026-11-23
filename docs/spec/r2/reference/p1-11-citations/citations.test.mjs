import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  verifyClaim, verifyAll, summarize, renderClaim, renderPage, squash,
  KINDS, NO_SOURCE_COPY, NO_SOURCE_HINT, PALETTE,
} from './citations.mjs';
import { buildSourceIndex, resolverFor, indexText } from './source-index.mjs';
import { ratio } from '../p0-05-card/contrast.mjs';

const read = (p) => readFileSync(new URL(p, import.meta.url), 'utf8');
const files = [
  { path: 'fixtures/note-a.md', text: read('./fixtures/note-a.md') },
  { path: 'fixtures/note-b.md', text: read('./fixtures/note-b.md') },
];
const claims = JSON.parse(read('./fixtures/claims.json')).claims;
const index = buildSourceIndex(files);
const resolve = resolverFor(index);
const byId = Object.fromEntries(verifyAll(claims, resolve).map((r) => [r.id, r]));

test('直接引用：逐字命中才算有据', () => {
  assert.equal(byId.c1.status, 'grounded');
  assert.equal(byId.c1.citations[0].verified, true);
  assert.equal(byId.c1.errors.length, 0);
});

test('引文对不上：状态是"来源对不上"，不是"没有来源"', () => {
  assert.equal(byId.c5.status, 'broken');
  assert.equal(byId.c5.fixedCopy, null);
  assert.ok(byId.c5.errors.some((e) => e.code === 'quote-not-in-source'));
});

test('锚点不存在：也是"来源对不上"', () => {
  assert.equal(byId.c6.status, 'broken');
  assert.ok(byId.c6.errors.some((e) => e.code === 'missing-anchor'));
});

test('完全没有引用：固定文案一字不差', () => {
  assert.equal(byId.c4.status, 'ungrounded');
  assert.equal(byId.c4.fixedCopy, NO_SOURCE_COPY);
  assert.ok(renderClaim(byId.c4).includes(NO_SOURCE_COPY));
  assert.ok(renderClaim(byId.c4).includes(NO_SOURCE_HINT));
});

test('只有转述：弱来源，不是有据', () => {
  assert.equal(byId.c2.status, 'weak');
});

test('只有推断：标成推断，并且必须打标签', () => {
  assert.equal(byId.c3.status, 'inferred');
  assert.equal(byId.c3.mustLabelInference, true);
  assert.ok(renderClaim(byId.c3).includes('含推断'));
});

test('未知引用类型与缺原文的直接引用都会被拒', () => {
  const a = verifyClaim({ id: 'x', text: 't', citations: [{ anchor: 'note-a-1', kind: 'vibes' }] }, resolve);
  assert.ok(a.errors.some((e) => e.code === 'bad-kind'));
  const b = verifyClaim({ id: 'y', text: 't', citations: [{ anchor: 'note-a-1', kind: 'direct' }] }, resolve);
  assert.ok(b.errors.some((e) => e.code === 'direct-without-quote'));
});

test('逐字核验忽略空白与换行', () => {
  const src = index.get('note-a-1').text;
  const spaced = src.slice(0, 2) + ' ' + src.slice(2) + String.fromCharCode(10);
  const r = verifyClaim({ id: 'z', text: 't', citations: [{ anchor: 'note-a-1', kind: 'direct', quote: spaced }] }, resolve);
  assert.equal(r.status, 'grounded');
  assert.equal(squash(' a b '), 'ab');
});

test('汇总：1 有据 / 1 弱 / 1 推断 / 1 无源 / 2 对不上，直接引用率 16.7%', () => {
  const s = summarize(Object.values(byId));
  assert.deepEqual(
    { total: s.total, grounded: s.grounded, weak: s.weak, inferred: s.inferred, ungrounded: s.ungrounded, broken: s.broken },
    { total: 6, grounded: 1, weak: 1, inferred: 1, ungrounded: 1, broken: 2 },
  );
  assert.equal(s.directRate, 16.7);
  assert.deepEqual(s.failures.map((f) => f.code).sort(), ['missing-anchor', 'no-source', 'quote-not-in-source']);
});

test('渲染转义：正文里的尖括号不会变成标签', () => {
  const r = verifyClaim({ id: 'h', text: '<script>alert(1)</script>', citations: [] }, resolve);
  const html = renderClaim(r);
  assert.ok(!html.includes('<script>'));
  assert.ok(html.includes('&lt;script&gt;'));
});

test('三信道：每条引用都有图标、文字标签、可点的锚点', () => {
  const html = renderClaim(byId.c1);
  const k = KINDS.direct;
  assert.ok(html.includes(k.label));
  assert.ok(html.includes('#' + k.icon));
  assert.ok(html.includes('href="#note-a-1"'));
});

test('三色对比度：边 ≥3:1、字 ≥4.5:1（纸与卡面）', () => {
  const paper = [.972, .006, 286], sheet = [.992, .003, 286];
  for (const [name, p] of Object.entries(PALETTE)) {
    assert.ok(ratio(p.line, paper) >= 3 && ratio(p.line, sheet) >= 3, name + ' 边色不达标: ' + ratio(p.line, paper).toFixed(2));
    assert.ok(ratio(p.ink, paper) >= 4.5 && ratio(p.ink, sheet) >= 4.5, name + ' 文字色不达标: ' + ratio(p.ink, paper).toFixed(2));
  }
});

test('源索引：重复锚点取第一处，非块行不算', () => {
  const map = indexText('x.md', '# 标题' + String.fromCharCode(10) + '正文 ^a-1' + String.fromCharCode(10) + '正文 ^a-1' + String.fromCharCode(10) + '没有锚点');
  assert.equal(map.size, 1);
  assert.equal(map.get('a-1').text, '正文');
});

test('整页渲染：状态统计与三条图例都在', () => {
  const html = renderPage(Object.values(byId));
  assert.ok(html.includes('直接引用率'));
  assert.ok(html.includes('推断永远不算证据'));
  assert.ok(html.includes('有据'));
});
test('预览页的"点回原文"是真链接：附录里每个锚点都有 id 与原文', () => {
  const html = renderPage(Object.values(byId));
  assert.ok(html.includes('id="note-a-1"'));
  assert.ok(html.includes('判断不是结论'));
  assert.ok(html.includes('href="#claim-c1"'));
});
