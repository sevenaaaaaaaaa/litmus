// 灵魂卡：解析 / 校验 / diff / 锚点重挂（P1-01）
// 三件事必须成立：
//   1) 解析得出六个区块与每条的块锚点（^belief-1 这种）
//   2) 改一个字，锚点不变（锚点属于"这一条判断"，不属于"这一行字符串"）
//   3) 新增一条才发新锚点，删掉的一条要能被 diff 看见
// 写法上刻意不用正则：锚点规则要能被测试逐字符验证，也免得转义出错。

export const SECTIONS = [
  { name: '我相信', key: 'belief', prefix: 'belief' },
  { name: '我如何判断', key: 'criteria', prefix: 'criteria' },
  { name: '我拒绝', key: 'refuse', prefix: 'refuse' },
  { name: '我的声音', key: 'voice', prefix: 'voice' },
  { name: '我在追的问题', key: 'question', prefix: 'question' },
  { name: '我的读者 / 作品', key: 'reader', prefix: 'reader', prefixes: ['reader', 'work'] },
  { name: '我的读者', key: 'reader', prefix: 'reader', prefixes: ['reader'] },
  { name: '我的作品', key: 'work', prefix: 'work' },
];

const byName = new Map(SECTIONS.map((s) => [s.name, s]));
export const sectionOf = (name) => byName.get(name) ?? null;

const NL = String.fromCharCode(10);
const ANCHOR_CHARS = 'abcdefghijklmnopqrstuvwxyz0123456789-_';

export function isAnchor(s) {
  if (typeof s !== 'string') return false;
  if (s.length < 3 || s.length > 48) return false;
  for (const ch of s) if (!ANCHOR_CHARS.includes(ch)) return false;
  if (!s.includes('-')) return false;          // 必须带前缀，否则分不清属于哪个区块
  if (s.startsWith('-') || s.endsWith('-')) return false;
  return true;
}

// 拆出 "文本 ^anchor"；没有锚点返回 anchor:null
export function splitAnchor(raw) {
  const i = raw.lastIndexOf('^');
  if (i < 0) return { text: raw.trim(), anchor: null };
  const anchor = raw.slice(i + 1).trim();
  const text = raw.slice(0, i).trimEnd();
  if (!anchor) return { text: raw.trim(), anchor: null };
  for (const ch of anchor) {
    if (ch === ' ' || ch === '\t' || ch === '\u3000') return { text: raw.trim(), anchor: null };
  }
  return { text, anchor };
}

function stripFence(lines) {
  if (lines[0]?.trim() !== '---') return { frontmatter: null, bodyStart: 0 };
  const end = lines.findIndex((l, i) => i > 0 && l.trim() === '---');
  if (end < 0) return { frontmatter: null, bodyStart: 0, broken: true };
  const fm = {};
  for (const l of lines.slice(1, end)) {
    const i = l.indexOf(':');
    if (i < 0) continue;
    fm[l.slice(0, i).trim()] = l.slice(i + 1).trim();
  }
  return { frontmatter: fm, bodyStart: end + 1 };
}

export function parseSoul(text) {
  const lines = text.split(NL);
  const { frontmatter, bodyStart } = stripFence(lines);
  const errors = [];
  const warnings = [];
  const sections = [];
  let cur = null;

  for (let i = bodyStart; i < lines.length; i++) {
    const line = lines[i];
    if (line.startsWith('## ')) {
      const name = line.slice(3).trim();
      const meta = sectionOf(name);
      if (!meta) {
        errors.push({ code: 'unknown-section', line: i + 1, message: '未知区块「' + name + '」' });
        cur = { name, key: null, items: [] };
      } else {
        cur = { name, key: meta.key, prefix: meta.prefix, prefixes: meta.prefixes ?? [meta.prefix], items: [] };
      }
      sections.push(cur);
      continue;
    }
    if (!cur) continue;
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (!trimmed.startsWith('- ')) {
      warnings.push({ code: 'non-item-line', line: i + 1, message: '区块里有一行不是项目符号，不会被灵魂卡读到' });
      continue;
    }
    const { text: body, anchor } = splitAnchor(trimmed.slice(2));
    if (!anchor) {
      errors.push({ code: 'missing-anchor', line: i + 1, message: '这一条没有块锚点：' + body.slice(0, 24) });
    } else if (!isAnchor(anchor)) {
      errors.push({ code: 'bad-anchor', line: i + 1, message: '块锚点不合规：^' + anchor });
    }
    cur.items.push({ anchor, text: body, line: i + 1 });
  }

  const doc = { frontmatter, sections, errors, warnings };
  validateSoul(doc);
  return doc;
}

export function validateSoul(doc) {
  const { sections } = doc;
  const errors = doc.errors;
  const warnings = doc.warnings;
  const seen = new Map();

  for (const s of sections) {
    if (!s.key) continue;
    if (s.items.length === 0) warnings.push({ code: 'empty-section', message: '区块「' + s.name + '」是空的' });
    for (const item of s.items) {
      if (!item.anchor) continue;
      if (seen.has(item.anchor)) {
        errors.push({ code: 'duplicate-anchor', line: item.line, message: '块锚点重复：^' + item.anchor + '（另一处在第 ' + seen.get(item.anchor) + ' 行）' });
      } else {
        seen.set(item.anchor, item.line);
      }
      const pfx = item.anchor.slice(0, item.anchor.lastIndexOf('-'));
      const allowed = s.prefixes ?? [s.prefix];
      if (!allowed.includes(pfx)) {
        warnings.push({ code: 'anchor-prefix', line: item.line, message: '锚点 ^' + item.anchor + ' 与区块前缀 ' + allowed.join('/') + ' 不一致（不致命，但会让 grep 变难）' });
      }
    }
  }

  const filled = sections.filter((s) => s.key && s.items.length > 0).length;
  if (filled === 0) errors.push({ code: 'empty-card', message: '这张卡没有任何内容' });
  else if (filled < 4) warnings.push({ code: 'thin-card', message: '只有 ' + filled + ' 个区块有内容，至少 4 个才算可用' });

  const confirmed = doc.frontmatter?.confirmed;
  doc.temporary = confirmed === 'false';
  doc.version = doc.frontmatter?.version ?? null;
  doc.anchorCount = seen.size;
  doc.itemCount = sections.reduce((n, s) => n + s.items.length, 0);
  return doc;
}

export function allAnchors(doc) {
  return doc.sections.flatMap((s) => s.items.filter((i) => i.anchor).map((i) => i.anchor));
}

export function diffSoul(oldDoc, newDoc) {
  const pick = (d) => {
    const m = new Map();
    for (const s of d.sections) for (const i of s.items) if (i.anchor) m.set(i.anchor, { text: i.text, section: s.name });
    return m;
  };
  const a = pick(oldDoc), b = pick(newDoc);
  const added = [], removed = [], edited = [];
  for (const [k, v] of b) {
    if (!a.has(k)) added.push({ anchor: k, text: v.text, section: v.section });
    else if (a.get(k).text !== v.text) edited.push({ anchor: k, from: a.get(k).text, to: v.text, section: v.section });
  }
  for (const [k, v] of a) if (!b.has(k)) removed.push({ anchor: k, text: v.text, section: v.section });
  return { added, removed, edited, unchanged: [...b.keys()].filter((k) => a.has(k) && a.get(k).text === b.get(k).text).length };
}

export function formatDiff(d) {
  const out = [];
  for (const e of d.edited) out.push('- 改 ^' + e.anchor + '：' + e.from + '  →  ' + e.to);
  for (const e of d.added) out.push('+ 新 ^' + e.anchor + '：' + e.text);
  for (const e of d.removed) out.push('- 删 ^' + e.anchor + '：' + e.text);
  out.push('（未变 ' + d.unchanged + ' 条）');
  return out.join(NL);
}

// ── 锚点重挂：用户改文字后，把锚点跟着"这一条"走，而不是跟着"这一行" ──
const norm = (line) => {
  const t = line.trim();
  const { text } = t.startsWith('- ') ? splitAnchor(t.slice(2)) : splitAnchor(t);
  return text;
};

function lcs(a, b) {
  const n = a.length, m = b.length;
  const dp = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const pairs = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { pairs.push([i, j]); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) i++;
    else j++;
  }
  return pairs;
}

const bigrams = (s) => {
  const clean = s.replace(/\s+/g, '');
  const out = [];
  for (let i = 0; i < clean.length - 1; i++) out.push(clean.slice(i, i + 2));
  return out;
};

// 字符二元组 Dice 相似度：0–1。用来判断"改了一两个字，还是同一条判断"。
export function dice(a, b) {
  const A = bigrams(a), B = bigrams(b);
  if (!A.length || !B.length) return 0;
  const pool = new Map();
  for (const g of A) pool.set(g, (pool.get(g) ?? 0) + 1);
  let hit = 0;
  for (const g of B) {
    const c = pool.get(g) ?? 0;
    if (c > 0) { hit++; pool.set(g, c - 1); }
  }
  return (2 * hit) / (A.length + B.length);
}

const prefixMap = (lines) => {
  let cur = null;
  return lines.map((l) => {
    if (l.startsWith('## ')) {
      const meta = sectionOf(l.slice(3).trim());
      cur = meta ? meta.prefix : null;
    }
    return cur;
  });
};

// 返回重挂后的全文 + 账：带过去几条、新铸几条、丢了几条
export function reanchor(oldText, newText) {
  const oldLines = oldText.split(NL);
  const newLines = newText.split(NL);

  // 旧文件的锚点，按"去掉锚点后的文本"建索引（同文多条时取第一条）
  const oldNorm = oldLines.map(norm);
  const oldAnchorAt = oldLines.map((l) => {
    const t = l.trim();
    const { anchor } = splitAnchor(t.startsWith('- ') ? t.slice(2) : t);
    return anchor;
  });
  const byText = new Map();
  oldNorm.forEach((t, i) => { if (oldAnchorAt[i] && !byText.has(t)) byText.set(t, oldAnchorAt[i]); });

  // 段落分隔：先把完全相同的行对上（LCS），剩下的按文本再兜一次
  const pairs = lcs(oldNorm, newLines.map(norm));
  const matchedOld = new Map();  // newIndex -> oldIndex
  for (const [oi, ni] of pairs) matchedOld.set(ni, oi);

  const usedAnchors = new Set();
  const carried = [];
  const minted = [];

  // 现有锚点（新文件自带的）先占位
  for (const l of newLines) {
    const t = l.trim();
    const { anchor } = splitAnchor(t.startsWith('- ') ? t.slice(2) : t);
    if (anchor) usedAnchors.add(anchor);
  }

  const prefixAt = (idx) => {
    let prefix = null;
    for (let i = 0; i <= idx; i++) {
      const l = newLines[i];
      if (l.startsWith('## ')) {
        const meta = sectionOf(l.slice(3).trim());
        prefix = meta ? meta.prefix : null;
      }
    }
    return prefix;
  };

  // 历史锚点也参与取号：删掉 voice-4 之后再新增，应该是 voice-5 而不是复用 4。
  // 复用会让老引用（卡背「读了哪几条」、已发布页面）悄悄指向另一条判断。
  const historical = new Set([...usedAnchors, ...oldAnchorAt.filter(Boolean)]);

  // 取号用「一次性播种、逐次自增」的计数器。每次重扫冻结的 historical 会让同一轮里
  // 新增的两条算出同一个 max + 1，铸出两个 voice-5（两条不同判断共用一个锚点，
  // 卡背与已发布页面的引用会静默指向另一条）。kernel-soul 的 max_num 同此语义。
  const seededMax = new Map();
  const nextNum = (prefix) => {
    if (seededMax.has(prefix)) {
      const v = seededMax.get(prefix) + 1;
      seededMax.set(prefix, v);
      return v;
    }
    let max = 0;
    for (const a of historical) {
      if (!a.startsWith(prefix + '-')) continue;
      const n = Number(a.slice(prefix.length + 1));
      if (Number.isFinite(n) && n > max) max = n;
    }
    seededMax.set(prefix, max + 1);
    return max + 1;
  };

  // LCS 只处理"整行相同"。改一两个字时行不同、但仍是同一条判断：
  // 按字符二元组相似度在同区块里配对，阈值 0.55（低于它更像是新写的一条）。
  {
    const oldPrefix = prefixMap(oldLines);
    const newPrefix = prefixMap(newLines);
    const takenOld = new Set(matchedOld.values());
    const spareOld = oldLines
      .map((l, i) => ({ i, anchor: oldAnchorAt[i], prefix: oldPrefix[i], text: oldNorm[i] }))
      .filter((o) => o.anchor && !takenOld.has(o.i) && !usedAnchors.has(o.anchor));

    for (let ni = 0; ni < newLines.length; ni++) {
      if (matchedOld.has(ni)) continue;
      const t = newLines[ni].trim();
      if (!t.startsWith('- ')) continue;
      const { anchor, text } = splitAnchor(t.slice(2));
      if (anchor || !text) continue;
      let best = -1, bestScore = 0;
      for (let k = 0; k < spareOld.length; k++) {
        const o = spareOld[k];
        if (o.prefix !== newPrefix[ni]) continue;
        const s = dice(text, o.text);
        if (s > bestScore) { bestScore = s; best = k; }
      }
      if (best >= 0 && bestScore >= 0.55) {
        const o = spareOld[best];
        matchedOld.set(ni, o.i);
        spareOld.splice(best, 1);
      }
    }
  }

  const out = newLines.map((line, idx) => {
    const t = line.trim();
    const isItem = t.startsWith('- ');
    const { text, anchor } = splitAnchor(isItem ? t.slice(2) : t);
    if (!isItem) return line;
    if (anchor) return line;                       // 已经有锚点，尊重它

    let carriedAnchor = null;
    const oi = matchedOld.get(idx);
    if (oi !== undefined && oldAnchorAt[oi] && !usedAnchors.has(oldAnchorAt[oi])) carriedAnchor = oldAnchorAt[oi];
    if (!carriedAnchor) {
      const hit = byText.get(text);
      if (hit && !usedAnchors.has(hit)) carriedAnchor = hit;
    }
    if (carriedAnchor) {
      usedAnchors.add(carriedAnchor);
      carried.push(carriedAnchor);
      return line.trimEnd() + ' ^' + carriedAnchor;
    }
    const prefix = prefixAt(idx) ?? 'item';
    const id = prefix + '-' + nextNum(prefix);
    usedAnchors.add(id);
    minted.push(id);
    return line.trimEnd() + ' ^' + id;
  });

  const newDoc = parseSoul(out.join(NL));
  const oldSet = new Set(oldAnchorAt.filter(Boolean));
  const newSet = new Set(allAnchors(newDoc));
  const deleted = [...oldSet].filter((a) => !newSet.has(a));

  return { text: out.join(NL), carried, minted, deleted };
}

// 「读了灵魂卡的哪几条」：把锚点还原成句子；未知锚点要显式说出来
export function resolveRefs(doc, ids) {
  const map = new Map();
  for (const s of doc.sections) for (const i of s.items) if (i.anchor) map.set(i.anchor, { text: i.text, section: s.name });
  const found = [], missing = [];
  for (const id of ids) {
    if (map.has(id)) found.push({ anchor: id, ...map.get(id) });
    else missing.push(id);
  }
  return { found, missing };
}
