// 五动作：validate / sheet / keys
// 用法：
//   node action-cli.mjs validate
//   node action-cli.mjs sheet [--out dist/reference.md]
//   node action-cli.mjs keys
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { validateSpec, renderSheet, normalizeKey } from './actions.mjs';

const here = new URL('.', import.meta.url);
const spec = JSON.parse(readFileSync(new URL('actions.spec.json', here), 'utf8'));
const events = JSON.parse(readFileSync(new URL('../p0-01-events/events.schema.json', here), 'utf8'));

const cmd = process.argv[2];
const outIdx = process.argv.indexOf('--out');
const out = outIdx > -1 ? process.argv[outIdx + 1] : null;

if (cmd === 'validate') {
  const r = validateSpec(spec, { events });
  for (const w of r.warnings) console.log('WARN  ' + w);
  for (const e of r.errors) console.log('ERROR ' + e);
  console.log(r.ok ? 'VALID ' + spec.actions.length + ' 个动作 / ' + spec.states.length + ' 个状态' : 'INVALID');
  process.exit(r.ok ? 0 : 1);
}

if (cmd === 'sheet') {
  const md = renderSheet(spec);
  if (out) {
    mkdirSync(new URL('dist/', here), { recursive: true });
    writeFileSync(new URL(out.replace(/^dist\//, ''), new URL('dist/', here)), md);
    console.log('写入 ' + out);
  } else process.stdout.write(md);
  process.exit(0);
}

if (cmd === 'keys') {
  for (const a of spec.actions) {
    const keys = Object.entries(a.keys).map(([p, ks]) => p + ':' + (ks.map(normalizeKey).join(',') || '—'));
    console.log(a.id.padEnd(8), keys.join('  '));
  }
  process.exit(0);
}

console.error('用法：validate | sheet [--out dist/reference.md] | keys');
process.exit(2);
