// P1-12：临时卡的出处（provenance）旁挂表。按块锚点键，落在 .litmus/soul-sources/ 下。
//
// 为什么不写进卡的 Markdown：
//   1. 出处是「每一条判断」一份，而卡的 frontmatter 是整卡一份的字符串表，装不下；
//   2. 硬塞就得改卡的格式——那要动 18 条对齐测试和两个逐字节对齐的 fixture；
//   3. 锚点稳定（P1-01 的第一条不变量），所以「按锚点键」的旁挂天然跟着卡走：
//      P1-10b 的升级只保证锚点是超集，旁挂自动仍然对得上。
//
// 为什么存 quote 而不只存 ref：ref 里的行号会漂、笔记会被改。
// 只有原话本身能让「这条判断最初是从哪句话来的」在半年后仍然可核。
//
// 三条判定全部「显式报，不静默补」：missing-source / dangling-source / unquoted-source。

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync, unlinkSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { parseSoul } from '../p1-01-soul/soul.mjs';
import { squash } from '../p1-11-citations/citations.mjs';

export const SOURCES_VERSION = 1;
export const SOURCES_DIR = '.litmus/soul-sources';

// 卡路径 → 旁挂路径。只转义 % 和 /，中文保持可读：
//   Notes/灵魂卡.md → .litmus/soul-sources/Notes%2F灵魂卡.json
// 必须可逆且不撞名——撞名等于两条判断共用一份出处，正是这一层要防的那类静默错误。
export function sourcesPath(cardRelPath) {
  const stem = cardRelPath.replace(/\.md$/i, '');
  const safe = stem.replace(/%/g, '%25').replace(/\//g, '%2F');
  return SOURCES_DIR + '/' + safe + '.json';
}

export function buildSources(cardRelPath, markdown, provenance) {
  const doc = parseSoul(markdown);
  if (doc.errors.length) throw new Error('卡不合法，不写旁挂：' + doc.errors.map((e) => e.message).join('; '));
  const anchors = {};
  const unmapped = [];
  for (const p of provenance || []) {
    const section = doc.sections.find((s) => s.key === p.block);
    // 一个区块只有一条判断才配得上一条区块级出处；多了就是「说不清是哪一条」，
    // 宁可报出来，也不随机挂到第一条上。
    if (!section || section.items.length !== 1 || !section.items[0].anchor) {
      unmapped.push(p.block);
      continue;
    }
    anchors[section.items[0].anchor] = {
      source: p.source || 'note',
      ref: p.ref,
      quote: p.quote,
    };
  }
  const sorted = {};
  for (const k of Object.keys(anchors).sort()) sorted[k] = anchors[k];
  return { sources: { version: SOURCES_VERSION, card: cardRelPath, anchors: sorted }, unmapped };
}

export function checkSources(markdown, sources) {
  const doc = parseSoul(markdown);
  const issues = [];
  const seen = new Set();
  for (const section of doc.sections) {
    for (const item of section.items) {
      if (!item.anchor) continue;
      seen.add(item.anchor);
      const entry = sources && sources.anchors ? sources.anchors[item.anchor] : null;
      if (!entry) {
        issues.push({ code: 'missing-source', line: item.line, message: item.anchor + ' 没有出处：答不出这条判断最初是从哪句话来的' });
      } else if (!squash(entry.quote).includes(squash(item.text))) {
        issues.push({ code: 'unquoted-source', line: item.line, message: item.anchor + ' 的 quote 不再覆盖条目正文（逐字可核失败）' });
      }
    }
  }
  for (const anchor of Object.keys((sources && sources.anchors) || {})) {
    if (!seen.has(anchor)) {
      issues.push({ code: 'dangling-source', message: anchor + ' 在旁挂里有、卡里没有：悬空出处不静默删' });
    }
  }
  return issues;
}

export function parseSources(json) {
  let s;
  try {
    s = JSON.parse(json);
  } catch (e) {
    throw new Error('bad-sources-json: 旁挂不是合法 JSON：' + e.message);
  }
  if (!s || typeof s !== 'object' || Array.isArray(s)) throw new Error('bad-sources-json: 旁挂不是一个对象');
  if (s.version !== SOURCES_VERSION) {
    throw new Error('unsupported-sources-version: 旁挂版本 ' + s.version + ' 不认识（这里只认 ' + SOURCES_VERSION + '）');
  }
  if (typeof s.card !== 'string' || typeof s.anchors !== 'object' || s.anchors === null) {
    throw new Error('bad-sources-json: 旁挂缺 card 或 anchors');
  }
  return s;
}

// 键是按锚点排序的，同一份内容两次序列化逐字节相同（版本库里不会抖出假 diff）。
export function sourcesToJson(sources) {
  return JSON.stringify(sources, null, 2) + String.fromCharCode(10);
}

export function writeSources(vault, cardRelPath, sources) {
  const rel = sourcesPath(cardRelPath);
  const target = join(vault, rel);
  mkdirSync(dirname(target), { recursive: true });
  const tmp = target + '.tmp-' + process.pid;
  writeFileSync(tmp, sourcesToJson(sources));
  renameSync(tmp, target);
  return { path: target, rel, anchors: Object.keys(sources.anchors).length };
}

// 读不到就明说读不到：返回 sources: null + error，不返回一个空表冒充「没有出处」。
export function readSources(vault, cardRelPath) {
  const rel = sourcesPath(cardRelPath);
  const target = join(vault, rel);
  if (!existsSync(target)) return { path: target, rel, sources: null, error: 'no-sources' };
  try {
    return { path: target, rel, sources: parseSources(readFileSync(target, 'utf8')), error: null };
  } catch (e) {
    return { path: target, rel, sources: null, error: e.message };
  }
}
