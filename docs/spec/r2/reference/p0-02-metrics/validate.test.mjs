import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { validate, checkEvents } from './validate.mjs';

const ev = JSON.parse(readFileSync(new URL('../p0-01-events/events.schema.json', import.meta.url), 'utf8'));
const mt = JSON.parse(readFileSync(new URL('./metrics.json', import.meta.url), 'utf8'));
const clone = (x) => JSON.parse(JSON.stringify(x));

test('真实文件：埋点与指标对得上', () => {
  const r = validate(ev, mt);
  assert.deepEqual(r.errors, []);
  assert.equal(r.ok, true);
});

test('指标引用不存在的事件会被抓出来', () => {
  const bad = clone(mt);
  bad.metrics[0].events.push('advance.nonexistent');
  const r = validate(ev, bad);
  assert.equal(r.ok, false);
  assert.ok(r.errors.some((e) => e.includes('不存在的事件')));
});

test('指标引用不存在的字段会被抓出来', () => {
  const bad = clone(mt);
  bad.metrics[0].needs.push('advance.settled.nope');
  const r = validate(ev, bad);
  assert.equal(r.ok, false);
  assert.ok(r.errors.some((e) => e.includes('字段不存在')));
});

test('没有事件支撑又没说明的指标会被抓出来', () => {
  const bad = clone(mt);
  const m = bad.metrics.find((x) => x.id === 'likeness');
  delete m.note;
  const r = validate(ev, bad);
  assert.equal(r.ok, false);
  assert.ok(r.errors.some((e) => e.includes('没有任何事件支撑')));
});

test('枚举少于两个值的事件属性会被抓出来', () => {
  const bad = clone(ev);
  bad.events[0].properties.phase.values = ['start'];
  const r = checkEvents(bad);
  assert.ok(r.errors.some((e) => e.includes('枚举少于两个值')));
});

test('pii 档位非法会被抓出来', () => {
  const bad = clone(ev);
  bad.events[0].pii = 'content';
  const r = checkEvents(bad);
  assert.ok(r.errors.some((e) => e.includes('pii 档位非法')));
});
test('role=metric 没指标消费会提醒；role=diagnostic 永不提醒', () => {
  const bad = clone(ev);
  bad.events.push({ name: 'probe.metric', when: 'x', pii: 'none', role: 'metric', properties: { a: { type: 'bool', required: true } } });
  bad.events.push({ name: 'probe.diag', when: 'x', pii: 'none', role: 'diagnostic', properties: { a: { type: 'bool', required: true } } });
  const r = validate(bad, mt);
  const orphan = r.warnings.find((w) => w.includes('role=metric 却没有指标消费')) ?? '';
  assert.ok(orphan.includes('probe.metric'));
  assert.ok(!orphan.includes('probe.diag'));
});

test('role 写错会被抓出来', () => {
  const bad = clone(ev);
  bad.events[0].role = 'nope';
  const r = checkEvents(bad);
  assert.ok(r.errors.some((e) => e.includes('role 非法')));
});
