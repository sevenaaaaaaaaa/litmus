// 端到端演示：在一个临时库里把推进闭环走一遍，并打印每一步的目录差异。
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { makeRun, attachCandidate, evaluate, commit, cancel, undo, snapshot, diffSnapshots } from './advance.mjs';

const NL = String.fromCharCode(10);
const vault = mkdtempSync(join(tmpdir(), 'litmus-demo-'));
mkdirSync(join(vault, 'Notes/写作'), { recursive: true });
writeFileSync(join(vault, 'Notes/写作/a.md'), '# A' + NL + NL + '判断不是结论，是被反复使用过的取舍。 ^a-1' + NL);
writeFileSync(join(vault, 'Notes/写作/b.md'), '# B' + NL + NL + '证据链的价值在于能点回原文。 ^b-1' + NL);

const SRC = {
  'a-1': { path: 'Notes/写作/a.md', text: '判断不是结论，是被反复使用过的取舍。' },
  'b-1': { path: 'Notes/写作/b.md', text: '证据链的价值在于能点回原文。' },
};
const resolve = (a) => SRC[a] ?? null;
const notes = () => snapshot(join(vault, 'Notes'));

console.log('库：' + vault);
const s0 = notes();

// 1) 取消：零写入
const r1 = makeRun(vault, { space: '写作', anchors: ['a-1'], intent: 'evidence', text: '找证据' });
attachCandidate(r1, { text: '不想要这句', citations: [{ anchor: 'a-1', kind: 'paraphrase' }] });
evaluate(r1, resolve);
cancel(r1);
console.log('1) 取消后 Notes 变化：' + JSON.stringify(diffSnapshots(s0, notes())));

// 2) 没有来源：闸门拦住
const r2 = makeRun(vault, { space: '写作', anchors: ['a-1'], intent: 'evidence', text: '空口一句' });
attachCandidate(r2, { text: '凭空来的结论', citations: [] });
const v2 = evaluate(r2, resolve);
console.log('2) 无来源 -> ' + r2.status + '（' + v2.blockReason + '）');

// 3) 正常闭环
const r3 = makeRun(vault, { space: '写作', anchors: ['a-1', 'b-1'], intent: 'evidence', text: '把判断推进一步' });
attachCandidate(r3, { text: '判断的价值在于它被反复使用。', citations: [{ anchor: 'a-1', kind: 'direct', quote: '是被反复使用过的取舍' }] });
const v3 = evaluate(r3, resolve);
console.log('3) 核验 -> ' + r3.status + '（' + v3.status + '）');
const out = commit(r3, resolve);
console.log('   落盘：' + out.cardPath.replace(vault + '/', '') + '　连回 ' + out.linked.join(','));
console.log('   来源卡末行：' + readFileSync(join(vault, 'Notes/写作/a.md'), 'utf8').trim().split(NL).pop());

// 4) 重试：幂等
const again = commit(r3, resolve);
console.log('4) 重试同一件事：duplicated=' + again.duplicated + '，卡片数=' + readdirSync(join(vault, 'Notes/写作')).filter((n) => n.startsWith('advance-')).length);

// 5) 撤销
undo(vault, out.writeKey);
console.log('5) 撤销后 Notes 与开始时：' + JSON.stringify(diffSnapshots(s0, notes())));
