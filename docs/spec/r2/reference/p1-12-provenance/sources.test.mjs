// P1-12：出处（provenance）旁挂表。用例与 Rust 侧 kernel/crates/kernel-soul/tests/sources.rs
// 同名同断言——两边跑同一批输入、同一批判据，才算这一层进了内核。

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, readdirSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  SOURCES_VERSION, sourcesPath, buildSources, checkSources, parseSources, sourcesToJson,
  writeSources, readSources,
} from './sources.mjs';
import { parseSoul, reanchor } from '../p1-01-soul/soul.mjs';
import {
  fromNotes, newReview, acceptAll, assemble, temporaryCard, writeCard,
} from '../p1-02-soul-entrances/entrances.mjs';

const read = (p) => readFileSync(new URL(p, import.meta.url), 'utf8');
const notes = [
  { path: 'Notes/note-1.md', text: read('../p1-02-soul-entrances/fixtures/note-1.md') },
  { path: 'Notes/note-2.md', text: read('../p1-02-soul-entrances/fixtures/note-2.md') },
];

const temp = () => temporaryCard(notes);
const coverAll = (markdown, provenance) => buildSources('Notes/灵魂卡.md', markdown, provenance).sources;

test('每条判断都有出处：checkSources 无问题', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  assert.equal(Object.keys(sources.anchors).length, parseSoul(markdown).itemCount, '每条判断都该有一条出处');
  assert.deepEqual(checkSources(markdown, sources), []);
});

test('旁挂序列化两次逐字节相同', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  const once = sourcesToJson(sources);
  const back = parseSources(once);
  assert.deepEqual(back, sources);
  assert.equal(sourcesToJson(back), once, '同一份内容两次序列化必须逐字节相同');
  assert.equal(back.version, SOURCES_VERSION);
  assert.equal(back.card, 'Notes/灵魂卡.md');
});

test('缺出处：显式报缺，不静默补', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  const first = Object.keys(sources.anchors)[0];
  delete sources.anchors[first];
  const found = checkSources(markdown, sources);
  assert.equal(found.length, 1);
  assert.equal(found[0].code, 'missing-source');
  assert.ok(found[0].line > 0, '要指到那条判断的行号');
  assert.equal(sources.anchors[first], undefined, '报缺不等于悄悄补上');
});

test('悬空出处：显式报，不静默删', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  sources.anchors['ghost-9'] = { source: 'paste', ref: 'paste:belief', quote: '一句卡里已经没有的话。' };
  const found = checkSources(markdown, sources);
  assert.deepEqual(found.map((i) => i.code), ['dangling-source']);
  assert.ok(sources.anchors['ghost-9'], '悬空出处不静默删');
});

test('quote 不再覆盖条目正文：逐字可核失败', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  const first = Object.keys(sources.anchors)[0];
  sources.anchors[first] = { source: 'interview', ref: 'answer:belief', quote: '一句和条目毫无关系的话。' };
  const found = checkSources(markdown, sources);
  assert.deepEqual(found.map((i) => i.code), ['unquoted-source']);
});

test('只差空白不算对不上', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  const first = Object.keys(sources.anchors)[0];
  const text = sources.anchors[first].quote;
  sources.anchors[first] = { ...sources.anchors[first], quote: '  ' + text.replace(/，/g, '， ') + ' ' + String.fromCharCode(10) + String.fromCharCode(0x3000) };
  assert.deepEqual(checkSources(markdown, sources), [], '只差空白不该判成对不上');
});

test('版本不认识：显式拒绝', () => {
  assert.throws(() => parseSources('{"version":2,"card":"Notes/a.md","anchors":{}}'), /unsupported-sources-version/);
});

test('坏 JSON：显式拒绝', () => {
  assert.throws(() => parseSources('{ 这不是 JSON'), /bad-sources-json/);
});

test('造卡即有出处：卡与旁挂一起落盘，读回来逐条对得上', () => {
  const vault = mkdtempSync(join(tmpdir(), 'litmus-prov-'));
  const { markdown, provenance } = temp();
  const { sources, unmapped } = buildSources('Notes/同名.md', markdown, provenance);
  assert.deepEqual(unmapped, [], '每一条出处都该有落点');
  writeCard(vault, markdown, { file: 'Notes/同名.md' });
  const w = writeSources(vault, 'Notes/同名.md', sources);
  assert.ok(existsSync(w.path));
  assert.equal(w.path, join(vault, sourcesPath('Notes/同名.md')));
  assert.equal(readdirSync(join(vault, '.litmus/soul-sources')).filter((n) => n.includes('.tmp-')).length, 0, '不留临时文件');
  const back = readSources(vault, 'Notes/同名.md');
  assert.equal(back.error, null);
  assert.deepEqual(checkSources(markdown, back.sources), []);
});

test('读不到旁挂就明说读不到，不拿空表冒充', () => {
  const vault = mkdtempSync(join(tmpdir(), 'litmus-prov-'));
  const back = readSources(vault, 'Notes/没有这张卡.md');
  assert.equal(back.sources, null);
  assert.equal(back.error, 'no-sources');
});

test('升级后新增的判断一定报缺出处，不会被旧旁挂冒充', () => {
  const { markdown, provenance } = temp();
  const sources = coverAll(markdown, provenance);
  const withNew = markdown.replace(/^(- .*\^voice-\d+)$/m, '$1' + String.fromCharCode(10) + '- 样本 9：升级时新写进来的一条。');
  const upgraded = reanchor(markdown, withNew).text;
  const found = checkSources(upgraded, sources);
  assert.equal(found.length, 1, '只该有一条新判断缺出处');
  assert.equal(found[0].code, 'missing-source');
});

test('一个区块里有多条时，区块级出处不硬挂到第一条', () => {
  const NL = String.fromCharCode(10);
  const { markdown, provenance } = temp();
  // 同一区块显式加第二条：区块级出处这时候说不清是哪一条，宁可报 unmapped。
  const md = markdown.replace(/^(- .*\^belief-1)$/m, '$1' + NL + '- 同一区块里的第二条判断。 ^belief-2');
  assert.deepEqual(parseSoul(md).errors, [], '造出来的卡本身要合法');
  const { sources, unmapped } = buildSources('Notes/多条目.md', md, provenance.filter((p) => p.block === 'belief'));
  assert.deepEqual(unmapped, ['belief'], '说不清是哪一条时应当报出来，而不是随机挂');
  assert.equal(sources.anchors['belief-1'], undefined, '不许硬挂到第一条');
});
