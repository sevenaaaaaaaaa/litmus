import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  parseSoul, diffSoul, reanchor, resolveRefs, isAnchor, allAnchors, dice,
} from './soul.mjs';

const ex = readFileSync(new URL('./soul-example.md', import.meta.url), 'utf8');
const tpl = readFileSync(new URL('./soul-template.md', import.meta.url), 'utf8');
const base = parseSoul(ex);

const stripAnchors = (text) => text.split(String.fromCharCode(10)).map((l) => {
  const i = l.lastIndexOf('^');
  if (i < 0) return l;
  return l.slice(0, i).trimEnd();
}).join(String.fromCharCode(10));

test('示例卡：六个区块、16 条、16 个锚点、无错误', () => {
  assert.equal(base.errors.length, 0);
  assert.equal(base.sections.length, 6);
  assert.equal(base.itemCount, 16);
  assert.equal(base.anchorCount, 16);
  assert.equal(base.temporary, false);
  assert.equal(base.version, '1');
});

test('所有锚点合法且唯一', () => {
  const anchors = allAnchors(base);
  assert.equal(new Set(anchors).size, anchors.length);
  for (const a of anchors) assert.ok(isAnchor(a), a + ' 不合法');
});

test('重复锚点是错误，并指出另一处行号', () => {
  const doc = parseSoul(ex.replace('^voice-2', '^voice-1'));
  const e = doc.errors.find((x) => x.code === 'duplicate-anchor');
  assert.ok(e, '没有抓到重复锚点');
  assert.ok(e.message.includes('voice-1'));
});

test('缺锚点是错误', () => {
  const doc = parseSoul(ex.replace('不写没有来源的结论。 ^refuse-3', '不写没有来源的结论。'));
  assert.ok(doc.errors.some((x) => x.code === 'missing-anchor'));
});

test('非法锚点（大写/空格）是错误', () => {
  assert.ok(parseSoul(ex.replace('^belief-1', '^BELIEF_1')).errors.some((x) => x.code === 'bad-anchor'));
  assert.ok(parseSoul(ex.replace('^belief-1', '^bad anchor')).errors.some((x) => x.code === 'missing-anchor'));
});

test('未知区块是错误', () => {
  const doc = parseSoul(ex + String.fromCharCode(10) + '## 我的星座' + String.fromCharCode(10) + '- 不许加。 ^zodiac-1' + String.fromCharCode(10));
  assert.ok(doc.errors.some((x) => x.code === 'unknown-section'));
});

test('空卡是错误；只有两个区块有内容时给提醒', () => {
  const empty = parseSoul('---' + String.fromCharCode(10) + 'type: soul-card' + String.fromCharCode(10) + '---' + String.fromCharCode(10));
  assert.ok(empty.errors.some((x) => x.code === 'empty-card'));
  const thin = parseSoul([
    '---', 'type: soul-card', 'confirmed: true', '---',
    '## 我相信', '- 一。 ^belief-1',
    '## 我拒绝', '- 二。 ^refuse-1',
  ].join(String.fromCharCode(10)));
  assert.ok(thin.warnings.some((x) => x.code === 'thin-card'));
});

test('临时卡：confirmed: false 会被标出来，且不是错误', () => {
  const doc = parseSoul(tpl);
  assert.equal(doc.temporary, true);
  assert.equal(doc.errors.length, 0);
});

test('diff：改一条只报一条 edited', () => {
  const next = parseSoul(ex.replace('知识不是收藏，是被用过的判断。', '知识不是收藏，是被反复用过的判断。'));
  const d = diffSoul(base, next);
  assert.equal(d.edited.length, 1);
  assert.equal(d.added.length, 0);
  assert.equal(d.removed.length, 0);
  assert.equal(d.edited[0].anchor, 'belief-1');
  assert.equal(d.unchanged, 15);
});

test('diff：加一条报 added，锚点正确', () => {
  const next = parseSoul(ex.replace('- 不写没有来源的结论。 ^refuse-3', '- 不写没有来源的结论。 ^refuse-3' + String.fromCharCode(10) + '- 不替别人下结论。 ^refuse-4'));
  const d = diffSoul(base, next);
  assert.equal(d.added.length, 1);
  assert.equal(d.added[0].anchor, 'refuse-4');
});

test('diff：删一条报 removed', () => {
  const next = parseSoul(ex.replace('- 不追热点：除非它和我的长期问题有关。 ^refuse-2' + String.fromCharCode(10), ''));
  const d = diffSoul(base, next);
  assert.equal(d.removed.length, 1);
  assert.equal(d.removed[0].anchor, 'refuse-2');
});

test('reanchor：改一个字，锚点一条不少地跟过去（零新铸）', () => {
  const mutated = stripAnchors(ex).replace('知识不是收藏，是被用过的判断。', '知识不是收藏，是被反复用过的判断。');
  const r = reanchor(ex, mutated);
  assert.equal(r.minted.length, 0, '不该新铸：' + r.minted.join(','));
  assert.equal(r.deleted.length, 0, '不该丢：' + r.deleted.join(','));
  const doc = parseSoul(r.text);
  assert.equal(doc.errors.length, 0);
  assert.equal(doc.anchorCount, 16);
  assert.ok(r.text.includes('被反复用过的判断。 ^belief-1'));
});

test('reanchor：新增一条只新铸一个，且前缀续号', () => {
  const withNew = stripAnchors(ex).replace(
    '- 特征：短句、具体、少形容词、先结论后理由。',
    '- 特征：短句、具体、少形容词、先结论后理由。' + String.fromCharCode(10) + '- 样本 4：写完先删掉三句。',
  );
  const r = reanchor(ex, withNew);
  assert.deepEqual(r.minted, ['voice-5']);
  assert.equal(r.deleted.length, 0);
});

test('锚点稳定性：40 次随机改字，原锚点全部保留', () => {
  let seed = 7;
  const rnd = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  const lines = ex.split(String.fromCharCode(10));
  const itemIdx = lines.map((l, i) => (l.trim().startsWith('- ') ? i : -1)).filter((i) => i >= 0);

  for (let round = 0; round < 40; round++) {
    const li = itemIdx[Math.floor(rnd() * itemIdx.length)];
    const line = lines[li];
    const caret = line.lastIndexOf('^');
    const body = line.slice(0, caret);
    // 只改正文，别把项目符号「- 」改掉（那会让这一行不再是条目）
    const pos = Math.max(3, body.indexOf('：') > 0 ? body.length - 3 : Math.floor(rnd() * (body.length - 2)) + 1);
    const mutatedLine = body.slice(0, pos) + '改' + body.slice(pos + 1);
    const newText = lines.map((l, i) => (i === li ? mutatedLine : l)).join(String.fromCharCode(10));
    const r = reanchor(ex, newText);
    const doc = parseSoul(r.text);
    assert.equal(doc.errors.length, 0, '第 ' + round + ' 轮产生错误');
    for (const a of allAnchors(base)) assert.ok(r.text.includes('^' + a), '第 ' + round + ' 轮丢了 ^' + a);
    assert.equal(r.minted.length, 0, '第 ' + round + ' 轮新铸了 ' + r.minted.join(','));
  }
});

test('resolveRefs：找得到句子，找不到的锚点显式列出', () => {
  const { found, missing } = resolveRefs(base, ['belief-1', 'nope-9']);
  assert.equal(found.length, 1);
  assert.equal(found[0].anchor, 'belief-1');
  assert.ok(found[0].text.includes('知识不是收藏'));
  assert.deepEqual(missing, ['nope-9']);
});

test('dice 相似度：同一句=1，完全无关<0.2', () => {
  assert.equal(dice('知识不是收藏，是被用过的判断。', '知识不是收藏，是被用过的判断。'), 1);
  assert.ok(dice('知识不是收藏，是被用过的判断。', '今天下午三点开会讨论排期。') < 0.2);
});
test('锚点编号不复用：删掉最大号再新增，拿到的是下一个号', () => {
  const NL = String.fromCharCode(10);
  const removed = ex.replace('- 特征：短句、具体、少形容词、先结论后理由。 ^voice-4' + NL, '');
  const added = removed.replace('- 样本 3：少形容词，多动词。 ^voice-3', '- 样本 3：少形容词，多动词。 ^voice-3' + NL + '- 样本 4：写完先删三句。');
  const r = reanchor(ex, stripAnchors(added));
  assert.ok(!r.minted.includes('voice-4'), '复用了已删除的 voice-4');
  assert.ok(r.minted.includes('voice-5'), '实际新铸：' + r.minted.join(','));
});

test('reanchor：同一轮新增两条，各拿各的号（不撞号）', () => {
  const NL = String.fromCharCode(10);
  const base = stripAnchors(ex).replace(
    '- 特征：短句、具体、少形容词、先结论后理由。',
    ['- 特征：短句、具体、少形容词、先结论后理由。', '- 样本 4：写完先删掉三句。', '- 样本 5：先给结论再给理由。'].join(NL),
  );
  const r = reanchor(ex, base);
  assert.equal(r.minted.length, 2, '应当只铸两条：' + r.minted.join(','));
  assert.equal(new Set(r.minted).size, 2, '两条新条目撞了同一个号：' + r.minted.join(','));
  const d = parseSoul(r.text);
  assert.deepEqual(d.errors, [], '撞号会变成重复锚点错误');
});
