import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, readdirSync, existsSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  fromInterview, fromNotes, fromPaste, newReview, decide, acceptAll, pending,
  assemble, temporaryCard, writeCard, assertGrounded, sentences, BLOCKS,
} from './entrances.mjs';
import { parseSoul } from '../p1-01-soul/soul.mjs';

const read = (p) => readFileSync(new URL(p, import.meta.url), 'utf8');
const notes = [
  { path: 'Notes/note-1.md', text: read('./fixtures/note-1.md') },
  { path: 'Notes/note-2.md', text: read('./fixtures/note-2.md') },
];

test('访谈入口：候选就是用户原话，逐字可核', () => {
  const cands = fromInterview({ belief: '知识不是收藏，是被用过的判断。好作品先有观点。' });
  assert.equal(cands.length, 2);
  for (const c of cands) assert.ok(c.provenance.quote.includes(c.text));
  assert.deepEqual(cands.map((c) => c.block), ['belief', 'belief']);
});

test('编造的候选会被拒（不是原句）', () => {
  assert.throws(() => assertGrounded({ text: 'agent 自己编的一句', provenance: { quote: '用户原话是另一句' } }), /不是原句/);
  assert.throws(() => assertGrounded({ text: '没有出处' }), /没有出处/);
});

test('提炼入口：带路径与行号，且每区块最多三条', () => {
  const cands = fromNotes(notes);
  assert.ok(cands.length >= 4);
  for (const c of cands) assert.match(c.provenance.ref, /^Notes\/note-\d\.md:\d+$/);
  const counts = {};
  for (const c of cands) counts[c.block] = (counts[c.block] ?? 0) + 1;
  for (const b of Object.keys(counts)) assert.ok(counts[b] <= 3, b + ' 超了三条');
});

test('提炼入口：标题、代码块、列表、链接都不进候选', () => {
  const texts = fromNotes(notes).map((c) => c.text);
  assert.ok(!texts.some((t) => t.startsWith('#')));
  assert.ok(!texts.some((t) => t.includes('代码块')));
  assert.ok(!texts.some((t) => t.startsWith('- ')));
});

test('粘贴入口：切句 + 逐字可核；未知区块报错', () => {
  const cands = fromPaste('我写东西先给结论。然后再补理由。', { block: 'voice' });
  assert.equal(cands.length, 2);
  assert.ok(cands.every((c) => c.block === 'voice'));
  assert.throws(() => fromPaste('随便', { block: 'nope' }), /未知区块/);
});

test('确认流程：接受/改写/删除，改写不能为空', () => {
  const review = newReview(fromInterview({ belief: '第一句原话在这里。第二句也在这里。' }));
  assert.equal(pending(review), 2);
  decide(review, 0, 'accepted');
  decide(review, 1, 'edited', '第二句被我改了一下');
  assert.equal(pending(review), 0);
  assert.throws(() => decide(review, 0, 'edited', '   '), /改写不能是空的/);
  assert.throws(() => decide(review, 0, 'maybe'), /未知决定/);
});

test('落盘格式：只写接受与改写的，锚点带前缀且唯一，解析无错', () => {
  const review = newReview(fromInterview({
    belief: '知识不是收藏，是被用过的判断。',
    refuse: '不写没有来源的结论。',
  }));
  decide(review, 0, 'accepted');
  decide(review, 1, 'dropped');
  const md = assemble(review, { confirmed: true });
  const doc = parseSoul(md);
  assert.deepEqual(doc.errors, []);
  assert.equal(doc.itemCount, 1);
  assert.ok(md.includes('^belief-1'));
  assert.ok(!md.includes('不写没有来源的结论'));
  assert.equal(doc.temporary, false);
});

test('未确认就是临时卡：confirmed:false 且解析能识别', () => {
  const review = acceptAll(newReview(fromInterview({ belief: '一句原话够不够长呢。' })));
  const doc = parseSoul(assemble(review, { confirmed: false }));
  assert.equal(doc.temporary, true);
});

test('三个入口共用同一条落盘路径：区块顺序与锚点前缀一致', () => {
  const a = assemble(acceptAll(newReview(fromInterview({ belief: '来自访谈的一句原话。' }))));
  const b = assemble(acceptAll(newReview(fromPaste('来自粘贴的一句旧文字。', { block: 'voice' }))));
  const c = assemble(acceptAll(newReview(fromNotes(notes))));
  const order = (md) => parseSoul(md).sections.map((s) => s.name);
  const rank = (n) => BLOCKS.findIndex((x) => x.name === n);
  for (const md of [a, b, c]) {
    const names = order(md).map(rank);
    assert.deepEqual(names, [...names].sort((x, y) => x - y), '区块顺序必须按 BLOCKS');
  }
});

test('临时卡：从旧笔记推断，confirmed 永远 false，且带出处', () => {
  const { markdown, provenance } = temporaryCard(notes);
  const doc = parseSoul(markdown);
  assert.equal(doc.temporary, true);
  assert.deepEqual(doc.errors, []);
  assert.ok(doc.itemCount >= 2);
  assert.ok(provenance.every((p) => p.ref.includes(':')));
});

test('写盘：原子写、拒绝覆盖、非法卡不落盘', () => {
  const v = mkdtempSync(join(tmpdir(), 'litmus-soul-'));
  const md = assemble(acceptAll(newReview(fromInterview({ belief: '一句真正来自用户的原话。' }))), { confirmed: true });
  const out = writeCard(v, md);
  assert.ok(existsSync(out.path));
  assert.equal(out.temporary, false);
  assert.equal(readdirSync(join(v, 'Notes')).filter((n) => n.includes('.tmp-')).length, 0);
  assert.throws(() => writeCard(v, md), /已经有灵魂卡/);
  const forced = writeCard(v, md, { force: true });
  assert.ok(forced.path);
  const empty = mkdtempSync(join(tmpdir(), 'litmus-soul-empty-'));
  assert.throws(() => writeCard(empty, '---' + String.fromCharCode(10) + 'type: soul-card' + String.fromCharCode(10) + '---' + String.fromCharCode(10)), /不合法/);
});

test('切句：中英标点与换行都算断句', () => {
  assert.deepEqual(sentences('一。二！三？四' + String.fromCharCode(10) + '五'), ['一', '二', '三', '四', '五']);
});
