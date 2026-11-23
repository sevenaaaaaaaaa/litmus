// 端到端演示：读 fixtures → 建索引 → 校验 → 打报告 → 生成预览页
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { verifyAll, summarize, renderPage } from './citations.mjs';
import { buildSourceIndex, resolverFor } from './source-index.mjs';

const here = new URL('.', import.meta.url);
const files = ['note-a.md', 'note-b.md'].map((f) => ({
  path: 'fixtures/' + f,
  text: readFileSync(new URL('fixtures/' + f, here), 'utf8'),
}));
const claims = JSON.parse(readFileSync(new URL('fixtures/claims.json', here), 'utf8')).claims;
const index = buildSourceIndex(files);
const resolve = resolverFor(index);
const results = verifyAll(claims, resolve);
const s = summarize(results);

for (const r of results) {
  const mark = r.status === 'grounded' ? 'OK  ' : r.status === 'ungrounded' ? 'NOSRC' : 'WEAK';
  console.log(mark + ' ' + r.id + '  ' + r.status + '  ' + (r.errors.map((e) => e.code).join(',') || '-'));
}
console.log('---');
console.log(JSON.stringify(s, null, 2));

mkdirSync(new URL('dist/', here), { recursive: true });
writeFileSync(new URL('dist/preview.html', here), renderPage(results));
console.log('预览：p1-11-citations/dist/preview.html');
process.exit(s.ungrounded === 1 ? 0 : 0);
