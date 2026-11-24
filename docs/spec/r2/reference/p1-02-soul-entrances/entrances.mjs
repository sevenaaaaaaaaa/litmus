// 灵魂卡三入口（P1-02）：访谈 / 提炼 / 粘贴
// 三条铁律：
//   1) 候选必须"有出处"——文字必须是用户原话或用户旧文里的原句（逐字子串）
//   2) 三个入口共用同一份确认流程与同一份落盘格式
//   3) 跳过也能用：生成临时卡（confirmed:false），每个区块标明来源
// 这一层不生成句子，只挑句子。生成会立刻变成"agent 替用户写卡"。
import fs from 'node:fs';
import { join, dirname } from 'node:path';
import { parseSoul } from '../p1-01-soul/soul.mjs';
import { squash } from '../p1-11-citations/citations.mjs';

const NL = String.fromCharCode(10);
export const BLOCKS = [
  { key: 'belief', name: '我相信', prefix: 'belief' },
  { key: 'criteria', name: '我如何判断', prefix: 'criteria' },
  { key: 'refuse', name: '我拒绝', prefix: 'refuse' },
  { key: 'voice', name: '我的声音', prefix: 'voice' },
  { key: 'question', name: '我在追的问题', prefix: 'question' },
  { key: 'reader', name: '我的读者 / 作品', prefix: 'reader' },
];

const byKey = new Map(BLOCKS.map((b) => [b.key, b]));
export const blockOf = (key) => byKey.get(key) ?? null;

export function assertGrounded(candidate) {
  if (!candidate.provenance || !candidate.provenance.quote) throw new Error('候选没有出处：' + candidate.text);
  if (!squash(candidate.provenance.quote).includes(squash(candidate.text))) {
    throw new Error('候选不是原句：' + candidate.text);
  }
  return candidate;
}

// 切句：中文句号/问号/叹号 + 换行。不做分词，不猜语义。
export function sentences(text) {
  const out = [];
  let buf = '';
  for (const ch of String(text ?? '')) {
    if (ch === '。' || ch === '！' || ch === '？' || ch === '!' || ch === '?' || ch === NL) {
      const t = buf.trim();
      if (t) out.push(t);
      buf = '';
    } else buf += ch;
  }
  const tail = buf.trim();
  if (tail) out.push(tail);
  return out;
}

const looksLikeNoise = (s) =>
  s.startsWith('#') || s.startsWith('>') || s.startsWith('|') || s.startsWith('- ') ||
  s.startsWith('\u0060\u0060\u0060') || s.startsWith('---') || s.includes('http') || s.length < 4 || s.length > 80;

// ── 入口一：访谈（用户原话就是候选） ──
export function fromInterview(answers) {
  const out = [];
  for (const [block, answer] of Object.entries(answers)) {
    if (!byKey.has(block)) throw new Error('未知区块：' + block);
    for (const s of sentences(answer)) {
      if (looksLikeNoise(s)) continue;
      out.push(assertGrounded({ block, text: s, provenance: { source: 'interview', ref: 'answer:' + block, quote: s } }));
    }
  }
  return out;
}

// ── 入口二：提炼（从旧笔记里挑句子，带路径与行号） ──
const CUES = {
  belief: ['不是', '其实是', '价值', '我认为', '我相信'],
  criteria: ['先看', '再看', '标准', '判断', '宁可'],
  refuse: ['不写', '拒绝', '不做', '不追'],
  voice: ['短句', '具体', '少形容词', '先结论'],
  question: ['如何', '为什么', '怎么才能', '一年'],
  reader: ['读者', '写给', '他们', '用户'],
};

export function fromNotes(files, { perBlock = 3 } = {}) {
  const out = [];
  for (const f of files) {
    const lines = String(f.text).split(NL);
    let inFence = false;
    lines.forEach((line, i) => {
      const t = line.trim();
      if (t.startsWith('\u0060\u0060\u0060')) { inFence = !inFence; return; }
      if (inFence) return;
      if (looksLikeNoise(t)) return;
      for (const s of sentences(line)) {
        if (looksLikeNoise(s)) continue;
        let best = null, score = 0;
        for (const [block, cues] of Object.entries(CUES)) {
          const hit = cues.filter((c) => s.includes(c)).length;
          if (hit > score) { score = hit; best = block; }
        }
        if (!best) return;
        out.push(assertGrounded({
          block: best, text: s,
          provenance: { source: 'note', ref: f.path + ':' + (i + 1), quote: s },
        }));
      }
    });
  }
  const seen = new Map();
  return out.filter((c) => {
    const n = seen.get(c.block) ?? 0;
    if (n >= perBlock) return false;
    seen.set(c.block, n + 1);
    return true;
  });
}

// ── 入口三：粘贴（自我介绍、旧文章、主页简介） ──
export function fromPaste(text, { block = 'voice' } = {}) {
  if (!byKey.has(block)) throw new Error('未知区块：' + block);
  return sentences(text)
    .filter((s) => !looksLikeNoise(s))
    .map((s) => assertGrounded({ block, text: s, provenance: { source: 'paste', ref: 'paste:' + block, quote: s } }));
}

// ── 共用：确认流程 ──
export function newReview(candidates) {
  return { items: candidates.map((c, i) => ({ ...c, index: i, decision: 'pending', edited: null })), confirmed: false };
}

export function decide(review, index, decision, editedText = null) {
  const item = review.items[index];
  if (!item) throw new Error('没有这一条：' + index);
  if (!['accepted', 'edited', 'dropped'].includes(decision)) throw new Error('未知决定：' + decision);
  if (decision === 'edited') {
    if (!editedText || !editedText.trim()) throw new Error('改写不能是空的');
    item.decision = 'edited';
    item.edited = editedText.trim();
  } else {
    item.decision = decision;
  }
  return item;
}

export function acceptAll(review) {
  for (const it of review.items) if (it.decision === 'pending') it.decision = 'accepted';
  return review;
}

export const pending = (review) => review.items.filter((i) => i.decision === 'pending').length;

// ── 共用：落盘格式（六个区块、块锚点、frontmatter） ──
export function assemble(review, { confirmed = false, version = 0, date = new Date().toISOString().slice(0, 10) } = {}) {
  const kept = review.items.filter((i) => i.decision === 'accepted' || i.decision === 'edited');
  const byBlock = new Map();
  for (const it of kept) {
    const list = byBlock.get(it.block) ?? [];
    list.push(it);
    byBlock.set(it.block, list);
  }
  const lines = [
    '---', 'type: soul-card', 'version: ' + version, 'updated: ' + date,
    'visibility: private', 'confirmed: ' + (confirmed ? 'true' : 'false'), '---', '',
  ];
  for (const b of BLOCKS) {
    const items = byBlock.get(b.key) ?? [];
    if (!items.length) continue;
    lines.push('## ' + b.name);
    items.forEach((it, i) => {
      const text = it.decision === 'edited' ? it.edited : it.text;
      lines.push('- ' + text + ' ^' + b.prefix + '-' + (i + 1));
    });
    lines.push('');
  }
  return lines.join(NL);
}

// 跳过路径：从旧笔记里推断一张临时卡；confirmed 永远 false
export function temporaryCard(files) {
  const cands = fromNotes(files, { perBlock: 1 });
  const review = acceptAll(newReview(cands));
  return {
    markdown: assemble(review, { confirmed: false }),
    // quote 与 source 一起带出来：旁挂要存原话，否则半年后 ref 里的行号一漂就无从核对。
    provenance: cands.map((c) => ({ block: c.block, ref: c.provenance.ref, quote: c.provenance.quote, source: c.provenance.source })),
  };
}

export function writeCard(vault, markdown, { file = 'Notes/灵魂卡.md', force = false } = {}) {
  const target = join(vault, file);
  if (fs.existsSync(target) && !force) throw new Error('已经有灵魂卡了，先备份或显式覆盖：' + file);
  const doc = parseSoul(markdown);
  if (doc.errors.length) throw new Error('生成的卡不合法：' + doc.errors.map((e) => e.message).join('; '));
  fs.mkdirSync(dirname(target), { recursive: true });
  const tmp = target + '.tmp-' + process.pid;
  fs.writeFileSync(tmp, markdown);
  fs.renameSync(tmp, target);
  return { path: target, temporary: doc.temporary, items: doc.itemCount, anchors: doc.anchorCount };
}
