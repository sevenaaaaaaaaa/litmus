import test from 'node:test';
import assert from 'node:assert/strict';
import { STATES, ORDER, reduce, activeAction, coverage, quickCheck, assertAccessible } from './card-state.mjs';

test('五个状态都有文字、图标、主动作（不靠颜色）', () => {
  for (const key of ORDER) {
    const a = assertAccessible(key);
    assert.equal(a.ok, true, key + ' 缺少: ' + a.missing.join(','));
    assert.ok(STATES[key].label.length > 0);
    assert.ok(STATES[key].action.length > 0);
  }
});

test('证据覆盖度沿主路径单调不减', () => {
  for (let i = 1; i < ORDER.length; i++) {
    assert.ok(coverage(ORDER[i]) >= coverage(ORDER[i - 1]), ORDER[i] + ' 覆盖度回落');
  }
  assert.equal(coverage('shipped'), 1);
});

test('正常路径：种子→有据→有张力→已定→已发布', () => {
  let s = 'seed';
  for (const ev of ['source.added', 'source.conflict', 'decide', 'publish']) {
    const r = reduce(s, ev);
    assert.equal(r.ok, true, ev + ' 被拒: ' + r.reason);
    s = r.state;
  }
  assert.equal(s, 'shipped');
});

test('没有来源不许发布，且给出理由', () => {
  const r = reduce('seed', 'publish');
  assert.equal(r.ok, false);
  assert.match(r.reason, /没有来源/);
});

test('有据但没做决定不许发布', () => {
  const r = reduce('grounded', 'publish');
  assert.equal(r.ok, false);
  assert.match(r.reason, /还没做决定/);
});

test('已发布可退回已定，其余不能撤回', () => {
  assert.equal(reduce('shipped', 'unpublish').state, 'settled');
  assert.equal(reduce('settled', 'unpublish').ok, false);
});

test('未知状态与未知事件直接报错，不静默', () => {
  assert.throws(() => reduce('ghost', 'decide'), /unknown state/);
  assert.throws(() => reduce('seed', 'teleport'), /unknown event/);
});

test('3 秒三问三句都可回答', () => {
  const q = quickCheck('tension', { space: '鹿蕊' });
  assert.match(q.where, /鹿蕊/);
  assert.match(q.state, /有张力/);
  assert.match(q.next, /做决定/);
  assert.ok(q.a11y.includes('有张力'));
});
