// 知识场壳（server/web/field.html）里的纯函数：不起浏览器，直接把函数源码抠出来在 vm 里跑。
// 跑法：node --test server/tests/field.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const html = fs.readFileSync(new URL('../web/field.html', import.meta.url), 'utf8');

/** 取顶层 `function name(` 的源码：函数体都缩进，到第一处顶格的 `}` 为止（一行写完的函数到行尾）。 */
function fn(name) {
  const start = html.indexOf('\nfunction ' + name + '(') + 1;
  assert.ok(start > 0, `找不到函数 ${name}`);
  const lineEnd = html.indexOf('\n', start);
  const first = html.slice(start, lineEnd);
  if (first.trimEnd().endsWith('}') && !first.trimEnd().endsWith('{')) {
    const opens = (first.match(/{/g) || []).length, closes = (first.match(/}/g) || []).length;
    if (opens === closes) return first;
  }
  const end = html.indexOf('\n}', start);
  return html.slice(start, end + 2);
}
function ctx(names, extra = '') {
  const c = vm.createContext({ esc: s => String(s).replace(/[&<>"]/g, ch => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[ch]) });
  vm.runInContext(extra + names.map(fn).join('\n'), c);
  return c;
}

test('intentOf：问、找、记、令、收、办、标', () => {
  const c = ctx(['intentOf'], "const AGENT_RE = /^(!|！|帮我|请帮我|请你|替我|让它|给我|帮忙)\\s*/;");
  const v = s => vm.runInContext(`intentOf(${JSON.stringify(s)})`, c);
  assert.equal(v('冲突的时候怎么处理？'), '问');
  assert.equal(v('同步 冲突'), '找');
  assert.equal(v('+ 刚想到的'), '记');
  assert.equal(v('/整理'), '令');
  assert.equal(v('https://example.com/a'), '收');
  assert.equal(v('帮我整理周报'), '办');
  assert.equal(v('#工作'), '标');
});

test('sectionsOf：按二级标题切节，没有小标题就按段落凑', () => {
  const c = ctx(['sectionsOf']);
  const s = vm.runInContext(`sectionsOf(${JSON.stringify('---\ntags: [a]\n---\n# 标题\n\n## 第一节\n\n' + '内容很长。'.repeat(10) + '\n\n## 第二节\n\n' + '另一段内容。'.repeat(10) + '\n\n```\n## 代码里的不算\n```\n')}, '标题')`, c);
  assert.equal(s.map(x => x.h).join('|'), '第一节|第二节');
  const p = vm.runInContext(`sectionsOf(${JSON.stringify('# 短文\n\n' + ('一句话。'.repeat(60) + '\n\n').repeat(4))}, '短文')`, c);
  assert.ok(p.length >= 2 && p[0].h.startsWith('短文（1）'));
});

test('addTags 与 setFm：只动 frontmatter，不碰正文', () => {
  const c = ctx(['addTags', 'setFm', 'setH1']);
  const r = (code) => vm.runInContext(code, c);
  assert.equal(r(`addTags('# 题\\n正文', ['甲'])`), '---\ntags: [甲]\n---\n# 题\n正文');
  assert.equal(r(`addTags('---\\ntags: [甲]\\n---\\n# 题', ['乙', '甲'])`), '---\ntags: [甲, 乙]\n---\n# 题');
  assert.equal(r(`setFm('---\\nstatus: x\\n---\\n正文', 'status', '待复核')`), '---\nstatus: 待复核\n---\n正文');
  assert.equal(r(`setH1('---\\na: 1\\n---\\n# 旧\\n正文', '新')`), '---\na: 1\n---\n# 新\n正文');
  assert.equal(r(`setH1('没有标题', '新')`), '# 新\n\n没有标题');
});

test('lineDiff：删的划掉、加的标出、长段不变的折叠', () => {
  const c = ctx(['lineDiff']);
  const a = ['# 题', ...Array.from({ length: 8 }, (_, i) => '不变 ' + i), '旧的一行'].join('\n');
  const b = ['# 题', ...Array.from({ length: 8 }, (_, i) => '不变 ' + i), '新的一行'].join('\n');
  const out = vm.runInContext(`lineDiff(${JSON.stringify(a)}, ${JSON.stringify(b)})`, c);
  assert.match(out, /<div class="dd">旧的一行<\/div>/);
  assert.match(out, /<div class="da">新的一行<\/div>/);
  assert.match(out, /行没变/);
});

test('shortLabel：场上的标签只留主干', () => {
  const c = ctx(['shortLabel']);
  const v = s => vm.runInContext(`shortLabel(${JSON.stringify(s)})`, c);
  assert.equal(v('ADR-0002 — 文件为内容真相，op-log 为并发真相'), 'ADR-0002 文件为内容真相');
  assert.equal(v('发布流程（新版）'), '发布流程');
  assert.ok(v('一'.repeat(40)).endsWith('…'));
});
