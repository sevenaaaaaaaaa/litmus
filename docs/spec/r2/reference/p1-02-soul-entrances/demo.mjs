// 三入口演示：访谈 / 提炼 / 粘贴 → 同一份确认 → 同一份落盘
import { mkdtempSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fromInterview, fromNotes, fromPaste, newReview, decide, acceptAll, pending, assemble, temporaryCard, writeCard } from './entrances.mjs';
import { parseSoul } from '../p1-01-soul/soul.mjs';

const read = (p) => readFileSync(new URL(p, import.meta.url), 'utf8');
const notes = [
  { path: 'Notes/note-1.md', text: read('./fixtures/note-1.md') },
  { path: 'Notes/note-2.md', text: read('./fixtures/note-2.md') },
];

const a = fromInterview({ belief: '知识不是收藏，是被用过的判断。好作品先有观点。', refuse: '不写没有来源的结论。' });
const b = fromNotes(notes);
const c = fromPaste('我写东西先给结论。然后再补理由。', { block: 'voice' });
console.log('访谈 ' + a.length + ' 条 / 提炼 ' + b.length + ' 条 / 粘贴 ' + c.length + ' 条');
console.log('三个入口的候选都逐字可核：' + [a, b, c].flat().every((x) => x.provenance.quote.includes(x.text)));

const review = newReview([...a, ...b, ...c]);
decide(review, 0, 'accepted');
decide(review, 1, 'edited', '好作品先有观点，再有结构。');
decide(review, 2, 'dropped');
acceptAll(review);
console.log('待确认 ' + pending(review) + ' 条');

const md = assemble(review, { confirmed: true, version: 1 });
const doc = parseSoul(md);
console.log('落盘前解析：' + doc.itemCount + ' 条 / ' + doc.anchorCount + ' 锚点 / 错误 ' + doc.errors.length + ' 条');

const vault = mkdtempSync(join(tmpdir(), 'litmus-soul-demo-'));
const out = writeCard(vault, md);
console.log('写入：' + out.path.replace(vault + '/', '') + '　临时卡=' + out.temporary);

const temp = temporaryCard(notes);
const tdoc = parseSoul(temp.markdown);
console.log('跳过路径：临时卡 ' + tdoc.itemCount + ' 条 / confirmed=' + !tdoc.temporary + ' / 出处 ' + temp.provenance.length + ' 条');
