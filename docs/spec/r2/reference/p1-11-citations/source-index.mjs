// 源块索引：把 Markdown 笔记里的块锚点读成 anchor -> 原文。
// 与灵魂卡共用同一套锚点语法（直接复用 P1-01 的 splitAnchor），
// 这样"引用"和"判断"用的是同一种可点回的地址。
import { splitAnchor } from '../p1-01-soul/soul.mjs';

export function indexText(path, text) {
  const out = new Map();
  const lines = text.split(String.fromCharCode(10));
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (!line) continue;
    const body = line.startsWith('- ') ? line.slice(2) : line;
    const { text: content, anchor } = splitAnchor(body);
    if (!anchor) continue;
    if (out.has(anchor)) continue;               // 同名锚点以第一处为准（重复另有检查）
    out.set(anchor, { path, text: content, line: i + 1 });
  }
  return out;
}

export function buildSourceIndex(files) {
  const merged = new Map();
  for (const f of files) for (const [k, v] of indexText(f.path, f.text)) if (!merged.has(k)) merged.set(k, v);
  return merged;
}

export const resolverFor = (index) => (anchor) => index.get(anchor) ?? null;
