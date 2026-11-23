// 灵魂卡命令行：parse / validate / diff / reanchor / refs
// 用法：
//   node soul-cli.mjs parse soul-example.md [--json]
//   node soul-cli.mjs validate soul-example.md
//   node soul-cli.mjs diff old.md new.md
//   node soul-cli.mjs reanchor old.md new.md [--out new-anchored.md]
//   node soul-cli.mjs refs soul-example.md belief-1 criteria-2
import { readFileSync, writeFileSync } from 'node:fs';
import { parseSoul, diffSoul, formatDiff, reanchor, resolveRefs, allAnchors } from './soul.mjs';

const [, , cmd, ...rest] = process.argv;

function flag(name) {
  const i = rest.indexOf('--' + name);
  if (i < 0) return null;
  const v = rest[i + 1] && !rest[i + 1].startsWith('--') ? rest[i + 1] : true;
  rest.splice(i, v === true ? 1 : 2);
  return v;
}

const json = flag('json');
const out = flag('out');
const files = rest.filter((x) => !x.startsWith('--'));
const read = (f) => readFileSync(f, 'utf8');
function die(msg) { console.error(msg); process.exit(2); }

if (cmd === 'parse') {
  const doc = parseSoul(read(files[0]));
  if (json) {
    console.log(JSON.stringify({
      version: doc.version,
      temporary: doc.temporary,
      itemCount: doc.itemCount,
      anchors: allAnchors(doc),
      sections: doc.sections.map((s) => ({ name: s.name, items: s.items.length })),
      errors: doc.errors,
      warnings: doc.warnings,
    }, null, 2));
  } else {
    console.log('版本 ' + (doc.version ?? '?') + (doc.temporary ? '（临时卡·未确认）' : ''));
    for (const s of doc.sections) console.log('## ' + s.name + '（' + s.items.length + ' 条）');
    console.log('锚点 ' + doc.anchorCount + ' 个 / 条目 ' + doc.itemCount + ' 条');
    for (const e of doc.errors) console.log('ERROR ' + e.message);
    for (const w of doc.warnings) console.log('WARN  ' + w.message);
  }
  process.exit(doc.errors.length ? 1 : 0);
}

if (cmd === 'validate') {
  const doc = parseSoul(read(files[0]));
  for (const e of doc.errors) console.log('ERROR 第' + (e.line ?? '?') + '行 ' + e.message);
  for (const w of doc.warnings) console.log('WARN  第' + (w.line ?? '?') + '行 ' + w.message);
  console.log(doc.errors.length ? 'INVALID' : 'VALID ' + doc.itemCount + ' 条 / ' + doc.anchorCount + ' 锚点');
  process.exit(doc.errors.length ? 1 : 0);
}

if (cmd === 'diff') {
  const d = diffSoul(parseSoul(read(files[0])), parseSoul(read(files[1])));
  console.log(formatDiff(d));
  process.exit(0);
}

if (cmd === 'reanchor') {
  const r = reanchor(read(files[0]), read(files[1]));
  if (typeof out === 'string') { writeFileSync(out, r.text); console.log('写入 ' + out); }
  else process.stdout.write(r.text);
  console.error('带过去 ' + r.carried.length + ' 条；新铸 ' + r.minted.length + ' 条'
    + (r.minted.length ? '（' + r.minted.join(', ') + '）' : '') + '；删除 ' + r.deleted.length + ' 条');
  process.exit(0);
}

if (cmd === 'refs') {
  const doc = parseSoul(read(files[0]));
  const ids = files.slice(1);
  const { found, missing } = resolveRefs(doc, ids);
  for (const f of found) console.log('^' + f.anchor + '（' + f.section + '）' + f.text);
  if (missing.length) console.log('没有找到：' + missing.map((m) => '^' + m).join(' '));
  process.exit(0);
}

die('用法：parse | validate | diff | reanchor | refs');
