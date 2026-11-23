import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readdirSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { openQueue, idFor } from './queue.mjs';

const FIXED = new Date('2026-11-23T13:04:00Z');
const make = (text, extra = {}) => ({
  id: idFor('text', text, FIXED), kind: 'text', text, note: null, createdAt: '2026-11-23T13:04:00Z', ...extra,
});

test('入队后可读回，字段不丢', () => {
  const q = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  assert.equal(q.enqueue(make('一句话')), true);
  const items = q.drain();
  assert.equal(items.length, 1);
  assert.equal(items[0].text, '一句话');
});

test('同一分钟同一内容只入队一次（幂等）', () => {
  const q = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  assert.equal(q.enqueue(make('重复')), true);
  assert.equal(q.enqueue(make('重复')), false);
  assert.equal(q.drain().length, 1);
});

test('按 createdAt 排序，同一时刻才用 id 打破平手', () => {
  const q = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  q.enqueue(make('晚一条', { createdAt: '2026-11-23T13:05:00Z' }));
  q.enqueue(make('早一条', { createdAt: '2026-11-23T13:04:00Z' }));
  assert.deepEqual(q.drain().map((i) => i.text), ['早一条', '晚一条']);
  const same = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  same.enqueue(make('A')); same.enqueue(make('B'));
  assert.equal(same.drain().length, 2);   // 同一时刻的顺序不做承诺
});

test('临时目录不留残片：成功入队后 .tmp 为空', () => {
  const q = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  q.enqueue(make('干净'));
  assert.equal(readdirSync(q.dirs.tmpDir).length, 0);
});

test('坏文件被跳过但不删除（不吞用户内容）', () => {
  const root = mkdtempSync(join(tmpdir(), 'litmus-q-'));
  const q = openQueue(root);
  q.enqueue(make('好的一条'));
  const bad = join(q.dirs.queueDir, '2026-11-23T13-05-00Z-deadbeef.json');
  writeFileSync(bad, '{ 这不是 JSON');
  const items = q.drain();
  assert.equal(items.length, 1);
  assert.equal(items[0].text, '好的一条');
  assert.ok(existsSync(bad), '坏文件必须留着');
});

test('按内容删除只删指定一条', () => {
  const q = openQueue(mkdtempSync(join(tmpdir(), 'litmus-q-')));
  const a = make('删除我'); q.enqueue(a); q.enqueue(make('留着我'));
  assert.equal(q.remove(a.id), true);
  assert.deepEqual(q.drain().map((i) => i.text), ['留着我']);
});