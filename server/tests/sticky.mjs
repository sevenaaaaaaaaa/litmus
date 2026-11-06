// 便利贴（server/web/sticky.html）里的纯函数：不起浏览器，直接把函数源码抠出来在 vm 里跑。
// 跑法：node --test server/tests/sticky.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const html = fs.readFileSync(new URL('../web/sticky.html', import.meta.url), 'utf8');

test('便利贴脚本可完整解析', () => {
  const m = html.match(/<script>([\s\S]*?)<\/script>/);
  assert.ok(m, '找不到便利贴脚本');
  assert.doesNotThrow(() => new vm.Script(m[1]));
});

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

const PRELUDE = "const STAMP_RE = /^\\d{4}-\\d{2}-\\d{2}[-_ ]?\\d{0,6}(-\\d{1,2})?$/;";

function ctx(names, extra = '') {
  const c = vm.createContext({});
  vm.runInContext(PRELUDE + '\n' + extra + '\n' + names.map(fn).join('\n'), c);
  return c;
}

/** 在 vm 里调一个纯函数；结果转回本 realm（vm 的数组原型不同，deepStrictEqual 会不认）。 */
function call(c, expr, ...args) {
  const out = vm.runInContext(`${expr}(${args.map(a => JSON.stringify(a)).join(', ')})`, c);
  return Array.isArray(out) ? Array.from(out, x => (x && typeof x === 'object' ? { ...x } : x)) : out;
}

test('splitFm：拆出 frontmatter 与正文', () => {
  const c = ctx(['splitFm']);
  assert.deepEqual(call(c, 'splitFm', '正文'), ['', '正文']);
  const parts = call(c, 'splitFm', '---\ndaily: true\n---\n# 标题');
  assert.equal(parts[0], '---\ndaily: true\n---\n');
  assert.equal(parts[1], '# 标题');
});

test('fmGet：读 frontmatter 值，缺了给 null', () => {
  const c = ctx(['splitFm', 'fmGet']);
  const md = '---\ndaily: true\ntags: "临时"\n---\n正文';
  assert.equal(call(c, 'fmGet', md, 'daily'), 'true');
  assert.equal(call(c, 'fmGet', md, 'tags'), '临时');
  assert.equal(call(c, 'fmGet', md, 'nope'), null);
  assert.equal(call(c, 'fmGet', '正文', 'daily'), null);
});

test('fmSet：加、改、删 daily 标记', () => {
  const c = ctx(['splitFm', 'fmSet']);
  assert.equal(call(c, 'fmSet', '正文', 'daily', 'true'), '---\ndaily: true\n---\n正文');
  assert.equal(
    call(c, 'fmSet', '---\ntags: a\n---\n正文', 'daily', 'true'),
    '---\ntags: a\ndaily: true\n---\n正文'
  );
  assert.equal(call(c, 'fmSet', '---\ndaily: true\n---\n正文', 'daily', null), '正文');
  assert.equal(
    call(c, 'fmSet', '---\ndaily: true\ntags: a\n---\n正文', 'daily', null),
    '---\ntags: a\n---\n正文'
  );
});

test('ensureDaily：新的一天在最上面另起一页，旧内容原样留下', () => {
  const c = ctx(['splitFm', 'ensureDaily'], 'function today() { return "2026-11-05"; }');
  assert.equal(call(c, 'ensureDaily', '', '2026-11-05'), '## 2026-11-05\n\n');
  assert.equal(call(c, 'ensureDaily', '## 2026-11-05\n\n今天', '2026-11-05'), '## 2026-11-05\n\n今天');
  assert.equal(
    call(c, 'ensureDaily', '## 2026-11-04\n\n昨天', '2026-11-05'),
    '## 2026-11-05\n\n## 2026-11-04\n\n昨天'
  );
  assert.equal(
    call(c, 'ensureDaily', '---\ndaily: true\n---\n## 2026-11-04\n\n昨天', '2026-11-05'),
    '---\ndaily: true\n---\n## 2026-11-05\n\n## 2026-11-04\n\n昨天'
  );
  assert.equal(
    call(c, 'ensureDaily', '# 每日便签\n\n## 2026-11-05\n\n今天', '2026-11-05'),
    '# 每日便签\n\n## 2026-11-05\n\n今天'
  );
});

test('noteLabel / isStampStem：认得出时间戳名', () => {
  const c = ctx(['noteLabel', 'isStampStem']);
  assert.equal(call(c, 'noteLabel', 'Notes/便利贴/2026-11-05-180000.md'), '2026-11-05-180000');
  assert.equal(call(c, 'isStampStem', 'Notes/便利贴/2026-11-05-180000.md'), true);
  assert.equal(call(c, 'isStampStem', 'Notes/便利贴/2026-11-05-180000-2.md'), true);
  assert.equal(call(c, 'isStampStem', 'Notes/便利贴/购物清单.md'), false);
  assert.equal(call(c, 'isStampStem', 'Notes/便利贴/2026-11-05-购物.md'), false);
});

test('noteTitle：取正文第一行像话的文字', () => {
  const c = ctx(['splitFm', 'noteTitle']);
  assert.equal(call(c, 'noteTitle', ''), '');
  assert.equal(call(c, 'noteTitle', '\n\n# 标题\n正文'), '标题');
  assert.equal(call(c, 'noteTitle', '---\ndaily: true\n---\n\n## 2026-11-05\n\n买牛奶\n'), '买牛奶');
  assert.equal(call(c, 'noteTitle', '## 2026-11-05\n\n- 交房租'), '交房租');
  assert.equal(call(c, 'noteTitle', '长'.repeat(60)), '长'.repeat(28));
});

test('fileStem：文件名去掉不允许的字符，限 40', () => {
  const c = ctx(['fileStem']);
  assert.equal(call(c, 'fileStem', '买牛奶'), '买牛奶');
  assert.equal(call(c, 'fileStem', 'a/b:c*d?e'), 'a-b-c-d-e');
  assert.equal(call(c, 'fileStem', '  -- 周会 纪要 --  '), '周会 纪要');
  assert.equal(call(c, 'fileStem', '   '), '');
  assert.equal(call(c, 'fileStem', '字'.repeat(60)).length, 40);
});

test('sortNotes：最近打开的在前，其余按改动时间', () => {
  const c = ctx(['sortNotes']);
  const list = [
    { path: 'Notes/便利贴/a.md', mtime: 50 },
    { path: 'Notes/便利贴/b.md', mtime: 10 },
    { path: 'Notes/便利贴/c.md', mtime: 30 }
  ];
  const order = rec => Array.from(call(c, 'sortNotes', list, rec)).map(x => x.path);
  assert.deepEqual(order(['Notes/便利贴/b.md']), [
    'Notes/便利贴/b.md', 'Notes/便利贴/a.md', 'Notes/便利贴/c.md'
  ]);
  assert.deepEqual(order([]), [
    'Notes/便利贴/a.md', 'Notes/便利贴/c.md', 'Notes/便利贴/b.md'
  ]);
});
