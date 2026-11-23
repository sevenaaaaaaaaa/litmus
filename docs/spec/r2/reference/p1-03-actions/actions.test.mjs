import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { validateSpec, renderSheet, normalizeKey } from './actions.mjs';
import { ORDER } from '../p0-05-card/card-state.mjs';

const read = (p) => JSON.parse(readFileSync(new URL(p, import.meta.url), 'utf8'));
const spec = read('./actions.spec.json');
const events = read('../p0-01-events/events.schema.json');
const clone = (x) => JSON.parse(JSON.stringify(x));
const run = (s) => validateSpec(s, { events });

test('真实规格通过：五个动作、五个状态、埋点全部存在', () => {
  const r = run(spec);
  assert.deepEqual(r.errors, []);
  assert.equal(r.ok, true);
  assert.equal(spec.actions.length, 5);
});

test('每个动作都有键盘键位与鼠标/触控方式（两个输入方式等价）', () => {
  for (const a of spec.actions) {
    const keys = Object.values(a.keys).flat().filter(Boolean);
    const gestures = Object.values(a.gestures).flat().filter(Boolean);
    assert.ok(keys.length >= 1, a.id + ' 没有键位');
    assert.ok(gestures.length >= 2, a.id + ' 触点太少');
  }
});

test('每个动作都有合法落点', () => {
  const allowed = ['new-card', 'existing-card', 'edge', 'doc', 'page', 'view'];
  for (const a of spec.actions) assert.ok(allowed.includes(a.landing), a.id + ' 落点非法');
});

test('会写盘的动作都可撤销、都保稿、都有可读的失败文案', () => {
  for (const a of spec.actions.filter((x) => x.writes)) {
    assert.equal(a.undoable, true, a.id);
    assert.equal(a.failure.preserveInput, true, a.id);
    assert.ok(a.failure.message.length > 6, a.id + ' 的失败文案太短');
    assert.equal(a.failure.noSilent, true, a.id);
  }
});

test('发布类落点必须二次确认', () => {
  const page = spec.actions.find((a) => a.landing === 'page');
  assert.equal(page.confirm, true);
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'ship').confirm = false;
  assert.ok(run(bad).errors.some((e) => e.includes('二次确认')));
});

test('键位冲突会被抓出来（同一平台同一组合）', () => {
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'ship').keys.mac = ['cmd+enter'];
  assert.ok(run(bad).errors.some((e) => e.includes('键位冲突')));
});

test('占用保留键会被抓出来', () => {
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'flip').keys.mac = ['cmd+k'];
  assert.ok(run(bad).errors.some((e) => e.includes('保留键')));
});

test('没有键盘键位会被抓出来', () => {
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'drop').keys = { mac: [], ios: [], web: [] };
  assert.ok(run(bad).errors.some((e) => e.includes('键盘不可达')));
});

test('落点缺失或非法会被抓出来', () => {
  const bad = clone(spec);
  delete bad.actions[0].landing;
  assert.ok(run(bad).errors.some((e) => e.includes('落点')));
});

test('写盘但不可撤销会被抓出来', () => {
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'link').undoable = false;
  assert.ok(run(bad).errors.some((e) => e.includes('不可撤销')));
});

test('埋点引用不存在的事件会被抓出来（跨模块对账）', () => {
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'advance').telemetry.push('advance.magic');
  assert.ok(run(bad).errors.some((e) => e.includes('不存在的事件')));
});

test('状态主动作必须恰好一个，且指向存在的动作', () => {
  const dup = clone(spec);
  dup.states.push({ state: 'seed', action: 'drop', intent: null, why: '多余的' });
  assert.ok(run(dup).errors.some((e) => e.includes('个数是 2')));
  const ghost = clone(spec);
  ghost.states[0].action = 'teleport';
  assert.ok(run(ghost).errors.some((e) => e.includes('不存在的动作')));
});

test('状态的 intent 必须落在埋点枚举里', () => {
  const bad = clone(spec);
  bad.states[0].intent = 'decide';
  assert.ok(run(bad).errors.some((e) => e.includes('intent')));
});

test('键位规范化：别名、顺序、大小写都不影响比较', () => {
  assert.equal(normalizeKey('Mod+Enter'), 'cmd+enter');
  assert.equal(normalizeKey('shift+cmd+enter'), normalizeKey('cmd+shift+enter'));
  assert.equal(normalizeKey('alt+space'), normalizeKey('opt+space'));
  assert.equal(normalizeKey('meta+k'), 'cmd+k');
});

test('五个状态名与卡片状态机保持一致（防两套名字漂移）', () => {
  assert.deepEqual(spec.states.map((s) => s.state), ORDER);
});

test('速查表包含五个动作、键位与失败文案', () => {
  const md = renderSheet(spec);
  for (const a of spec.actions) {
    assert.ok(md.includes(a.label), a.label + ' 没进速查表');
    assert.ok(md.includes(a.failure.message), a.id + ' 的失败文案没进表');
  }
  assert.ok(md.includes('保留键'));
  assert.ok(!md.includes('更多菜单'));
});
test('等价是逐操作检查的：每个操作键盘与指针都必须可达', () => {
  for (const a of spec.actions) {
    assert.ok(a.ops.length >= 2, a.id + ' 操作太少');
    for (const op of a.ops) {
      assert.equal(op.keyboard, true, a.id + '/' + op.name + ' 键盘不可达');
      assert.equal(op.pointer, true, a.id + '/' + op.name + ' 指针不可达');
    }
  }
  const bad = clone(spec);
  bad.actions.find((a) => a.id === 'advance').ops[2].pointer = false;
  assert.ok(run(bad).errors.some((e) => e.includes('双向可达')));
});
