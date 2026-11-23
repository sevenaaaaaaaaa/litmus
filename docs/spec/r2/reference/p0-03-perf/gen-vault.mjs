// 生成一个「1 万篇 / 10 万块」量级的合成库，用来压性能，不碰真库。
// 用法：node gen-vault.mjs --out /tmp/perf-vault --notes 10000
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const arg = (name, fallback) => {
  const i = process.argv.indexOf('--' + name);
  return i > -1 ? process.argv[i + 1] : fallback;
};

const out = arg('out', '/tmp/litmus-perf-vault');
const count = Number(arg('notes', 2000));
const perNoteBlocks = Number(arg('blocks', 10));
const NL = String.fromCharCode(10);

// 词表刻意混合中英与代码块，避免只测「纯文本」这种最好情况。
const ZH = ['判断', '来源', '反例', '显色', '线索', '推进', '灵魂卡', '证据', '画布', '卡片', '张力', '已定'];
const EN = ['claim', 'source', 'counterexample', 'canvas', 'advance', 'soul', 'evidence', 'publish'];
const pick = (arr, i) => arr[i % arr.length];

function note(i) {
  const paras = [];
  for (let b = 0; b < perNoteBlocks; b++) {
    const zh = Array.from({ length: 18 }, (_, k) => pick(ZH, i + b + k)).join('');
    const en = Array.from({ length: 8 }, (_, k) => pick(EN, i * 3 + b + k)).join(' ');
    paras.push(`## 块 ${b + 1} · ${pick(ZH, i + b)}

${zh}。这是第 ${i} 篇第 ${b + 1} 块，用来占位。

> ${en}

- [ ] 待办 ${b}
- 关联：[[Note-${(i + b + 1) % count}]]`);
  }
  return `---
title: 合成笔记 ${i}
tags: [perf, synthetic]
created: 2026-11-23
---

# 合成笔记 ${i}

${paras.join(NL + NL)}
`;
}

const t0 = process.hrtime.bigint();
mkdirSync(join(out, 'Notes'), { recursive: true });
let bytes = 0;
for (let i = 0; i < count; i++) {
  const body = note(i);
  bytes += Buffer.byteLength(body);
  writeFileSync(join(out, 'Notes', `Note-${i}.md`), body);
}
const ms = Number(process.hrtime.bigint() - t0) / 1e6;
console.log(JSON.stringify({
  out, notes: count, blocks_per_note: perNoteBlocks,
  blocks_total: count * perNoteBlocks,
  written_mb: +(bytes / 1048576).toFixed(1),
  gen_ms: +ms.toFixed(0),
}, null, 2));
