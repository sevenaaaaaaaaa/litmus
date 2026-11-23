// 同一个 run，走两个后端：一个真跑，一个显式失败。
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { FileBackend, EndpointBackend, selectBackend } from './backends.mjs';
import { makeRun, attachCandidate, evaluate } from '../p1-04-advance/advance.mjs';

const NL = String.fromCharCode(10);
const v = mkdtempSync(join(tmpdir(), 'litmus-be-demo-'));
mkdirSync(join(v, 'Notes/写作'), { recursive: true });
writeFileSync(join(v, 'Notes/写作/a.md'), '# A' + NL + NL + '判断不是结论，是被反复使用过的取舍。 ^a-1' + NL);
const resolve = (a) => (a === 'a-1' ? { path: 'Notes/写作/a.md', text: '判断不是结论，是被反复使用过的取舍。' } : null);

const r = makeRun(v, { space: '写作', anchors: ['a-1'], intent: 'evidence', text: '找证据' });
attachCandidate(r, { text: '判断的价值在于被反复使用。', citations: [{ anchor: 'a-1', kind: 'direct', quote: '是被反复使用过的取舍' }] });
evaluate(r, resolve);

const file = FileBackend(v);
console.log('文件后端 describe：' + JSON.stringify(file.describe()));
const out = file.commit(r, resolve);
console.log('文件后端落盘：' + out.cardPath.replace(v + '/', '') + '，撤销=' + JSON.stringify(file.undo(out.writeKey)));

const ep = EndpointBackend();
console.log('端点后端 describe：' + JSON.stringify(ep.describe()));
try { ep.commit(); } catch (e) { console.log('端点后端失败（预期）：' + e.code + ' 缺 ' + e.needs.join(',')); }

try { selectBackend({ vault: v, endpoint: { mode: 'endpoint' } }); }
catch (e) { console.log('选后端失败（预期，不降级）：' + e.code); }
