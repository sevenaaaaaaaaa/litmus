// 最小 DOM 替身：只验证 bundle 能跑完一次渲染、不抛错、卡片数正确。
// 不是浏览器测试，但能挡住「少一个元素/属性名写错」这类低级错误。
import test from 'node:test';
import assert from 'node:assert/strict';

const nodes = new Map();
function el(id) {
  const node = {
    id, innerHTML: '', hidden: false, textContent: '', dataset: {},
    classList: { add() {}, remove() {}, toggle() {} },
    addEventListener() {}, querySelectorAll() { return []; },
    querySelector() { return null; },
  };
  nodes.set(id, node);
  return node;
}
for (const id of ['stack', 'toast', 'checkPanel', 'colorOnly', 'check', 'canvas']) el(id);
nodes.get('colorOnly').checked = false;

globalThis.document = {
  getElementById: (id) => nodes.get(id) ?? null,
  querySelectorAll: () => [],
  addEventListener() {},
  body: { dataset: {} },
};
globalThis.window = globalThis;

test('bundle 能完成一次渲染，三张卡都在', async () => {
  await import('./dist/prototype.js');
  const html = nodes.get('stack').innerHTML;
  assert.match(html, /灵魂卡不是人设/);
  assert.match(html, /3 秒落下/);
  assert.match(html, /先有画布/);
  assert.equal((html.match(/class="card"/g) || []).length, 3);
  // 状态文字必须出现在正面，证明不是只靠颜色
  assert.match(html, /有张力 · 与另一条证据或判断冲突/);
});
