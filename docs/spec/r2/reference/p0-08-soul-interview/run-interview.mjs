// 打印一份 10 分钟访谈台本；也校验每个问题都有候选与追问。
// 用法：node run-interview.mjs [--compact]
import { readFileSync } from 'node:fs';
const doc = JSON.parse(readFileSync(new URL('./questions.json', import.meta.url), 'utf8'));
const compact = process.argv.includes('--compact');

let clock = 0;
const mmss = (s) => String(Math.floor(s / 60)).padStart(2, '0') + ':' + String(s % 60).padStart(2, '0');

if (!compact) {
  console.log('# 鹿蕊 · 灵魂卡访谈台本（10 分钟）');
  console.log('目标：' + doc.goal + '\n');
  console.log('规则：');
  for (const r of doc.rules) console.log('  · ' + r);
  console.log('\n---\n');
}
for (const q of doc.questions) {
  const t = mmss(clock);
  clock += q.seconds;
  console.log(`[${t}] ${q.block}（${q.id}，${q.seconds}s）`);
  console.log('  问：' + q.ask);
  for (const c of q.candidates) console.log('  候选：' + c);
  console.log('  追问：' + q.probe);
  console.log('');
}
console.log('合计 ' + mmss(clock) + '。收尾：逐条确认 → 写 Notes/灵魂卡.md → 标注「已确认」。');
console.log('判定：' + doc.scoring.done);
console.log('放弃：' + doc.scoring.abandon);
