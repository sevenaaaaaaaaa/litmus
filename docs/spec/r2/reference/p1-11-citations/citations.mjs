// 证据与三色引用（P1-11）
// 一条结论只有三种可信状态，多种失败要说清楚是哪一种：
//   direct     逐字出现在源块里（程序可验证）
//   paraphrase 来自源块，但换了说法
//   inference  模型推出来的，必须显式标注，永远不算证据
// 失败也分三种：没有来源 / 来源对不上 / 只有推断。合并成一句"没有找到来源"就是在撒谎。

export const KINDS = {
  direct: { key: 'direct', label: '直接引用', short: '引', icon: 'i-quote', tone: 'cite-direct', proof: true },
  paraphrase: { key: 'paraphrase', label: '转述', short: '述', icon: 'i-paraphrase', tone: 'cite-para', proof: false },
  inference: { key: 'inference', label: '推断', short: '推', icon: 'i-infer', tone: 'cite-infer', proof: false },
};

// 固定文案：改一个字都算产品事故，用户靠这句话判断"能不能信"。
export const NO_SOURCE_COPY = '没有找到来源';
export const NO_SOURCE_HINT = '这句话没有可点回的出处，先别当成结论用。';

// 三色的 oklch（tokens 里同名；测试会算对比度）
export const PALETTE = {
  direct: { line: [.53, .19, 259], ink: [.45, .17, 259] },
  paraphrase: { line: [.47, .02, 282], ink: [.40, .02, 282] },
  inference: { line: [.57, .19, 18], ink: [.50, .19, 18] },
};

const isSpace = (ch) => ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r' || ch === '\u3000';
export const squash = (s) => {
  if (typeof s !== 'string') return '';
  let out = '';
  for (const ch of s) if (!isSpace(ch)) out += ch;
  return out;
};

const esc = (s) => {
  let out = '';
  for (const ch of String(s)) {
    if (ch === '&') out += '&amp;';
    else if (ch === '<') out += '&lt;';
    else if (ch === '>') out += '&gt;';
    else if (ch === '"') out += '&quot;';
    else out += ch;
  }
  return out;
};

// claim = { id, text, citations: [{ anchor, kind, quote? }] }
// resolve(anchor) -> { text, path } | null
export function verifyClaim(claim, resolve) {
  const errors = [];
  const citations = Array.isArray(claim.citations) ? claim.citations : [];
  const checked = [];

  if (!claim.id) errors.push({ code: 'no-id', message: '结论没有 id' });
  if (!claim.text || !claim.text.trim()) errors.push({ code: 'empty-text', message: '结论没有正文' });
  if (citations.length === 0) errors.push({ code: 'no-source', message: NO_SOURCE_COPY });

  for (const c of citations) {
    const kind = KINDS[c.kind];
    if (!kind) { errors.push({ code: 'bad-kind', anchor: c.anchor, message: '未知的引用类型：' + c.kind }); continue; }
    const src = c.anchor ? resolve(c.anchor) : null;
    if (!src) {
      errors.push({ code: 'missing-anchor', anchor: c.anchor, message: '锚点不存在：^' + (c.anchor ?? '') });
      continue;
    }
    let verified = false;
    if (kind.key === 'direct') {
      if (!c.quote || !c.quote.trim()) {
        errors.push({ code: 'direct-without-quote', anchor: c.anchor, message: '直接引用必须带原文，否则只能算转述' });
      } else if (!squash(src.text).includes(squash(c.quote))) {
        errors.push({ code: 'quote-not-in-source', anchor: c.anchor, message: '原文里找不到这句话：' + c.quote });
      } else {
        verified = true;
      }
    } else {
      verified = true;
    }
    checked.push({ anchor: c.anchor, kind: kind.key, quote: c.quote ?? null, verified, source: src.path, sourceText: src.text });
  }

  const hasDirect = checked.some((c) => c.kind === 'direct' && c.verified);
  const hasParaphrase = checked.some((c) => c.kind === 'paraphrase' && c.verified);
  const hasInference = checked.some((c) => c.kind === 'inference');
  const anyVerified = checked.some((c) => c.verified);

  const status = citations.length === 0
    ? 'ungrounded'
    : !anyVerified ? 'broken'
    : hasDirect ? 'grounded'
    : hasParaphrase ? 'weak'
    : hasInference ? 'inferred'
    : 'ungrounded';

  return {
    id: claim.id ?? null,
    text: claim.text ?? '',
    status,
    mustLabelInference: hasInference,
    citations: checked,
    errors,
    ok: errors.length === 0,
    fixedCopy: status === 'ungrounded' ? NO_SOURCE_COPY : null,
  };
}

export function verifyAll(claims, resolve) {
  return claims.map((c) => verifyClaim(c, resolve));
}

export function summarize(results) {
  const by = (s) => results.filter((r) => r.status === s).length;
  const codes = new Map();
  for (const r of results) for (const e of r.errors) codes.set(e.code, (codes.get(e.code) ?? 0) + 1);
  const total = results.length;
  const grounded = by('grounded');
  return {
    total,
    grounded,
    weak: by('weak'),
    inferred: by('inferred'),
    ungrounded: by('ungrounded'),
    broken: by('broken'),
    directRate: total ? +(grounded / total * 100).toFixed(1) : 0,
    failures: [...codes.entries()].map(([code, n]) => ({ code, n })).sort((a, b) => b.n - a.n),
  };
}

// 每条引用都要"颜色 + 图标 + 文字"三信道；只给颜色等于没标。
export function renderClaim(result) {
  const chips = result.citations.map((c) => {
    const k = KINDS[c.kind];
    const bad = !c.verified;
    return '<span class="chip ' + k.tone + (bad ? ' bad' : '') + '" data-kind="' + k.key + '">'
      + '<svg class="ico"><use href="#' + k.icon + '"></use></svg>'
      + '<b>' + esc(k.label) + '</b>'
      + '<a href="#' + esc(c.anchor) + '">^' + esc(c.anchor) + '</a>'
      + (c.quote ? '<q>' + esc(c.quote) + '</q>' : '')
      + (bad ? '<em>未通过核验</em>' : '')
      + '</span>';
  }).join('');

  const badge = result.mustLabelInference
    ? '<span class="badge cite-infer-ink"><svg class="ico"><use href="#i-infer"></use></svg>含推断</span>'
    : '';

  const body = result.status === 'ungrounded'
    ? '<p class="claim-text muted">' + esc(result.text) + '</p>'
      + '<p class="no-source"><b>' + esc(NO_SOURCE_COPY) + '</b>　' + esc(NO_SOURCE_HINT) + '</p>'
    : '<p class="claim-text">' + esc(result.text) + '</p>';

  const problems = result.errors.length
    ? '<ul class="problems">' + result.errors.map((e) => '<li>' + esc(e.message) + '</li>').join('') + '</ul>'
    : '';

  return '<article class="claim" data-status="' + result.status + '" id="claim-' + esc(result.id ?? '') + '">'
    + '<header><span class="status ' + result.status + '">' + statusLabel(result.status) + '</span>' + badge + '</header>'
    + body
    + (chips ? '<div class="chips">' + chips + '</div>' : '')
    + problems
    + '</article>';
}

// 附录：把被引用过的源块原文列出来，锚点 id 就是链接目标，
// 这样"点回原文"在预览页里是真的能点的，不是只有下划线。
function appendix(results) {
  const seen = new Map();
  for (const r of results) for (const c of r.citations) if (!seen.has(c.anchor)) seen.set(c.anchor, c);
  if (!seen.size) return '';
  const rows = [...seen.values()].map((c) =>
    '<li id="' + esc(c.anchor) + '"><a class="ax" href="#claim-' + esc(resultIdForAnchor(results, c.anchor)) + '">^' + esc(c.anchor) + '</a>'
    + '<span class="src">' + esc(c.sourceText ?? '') + '</span>'
    + '<em class="file">' + esc(c.source ?? '') + '</em></li>').join('');
  return '<section class="appendix"><h3>被引用的原文</h3><ul>' + rows + '</ul></section>';
}

function resultIdForAnchor(results, anchor) {
  const hit = results.find((r) => r.citations.some((c) => c.anchor === anchor));
  return hit?.id ?? '';
}

export function statusLabel(status) {
  return { grounded: '有据', weak: '只有转述', inferred: '只有推断', ungrounded: '没有来源', broken: '来源对不上' }[status] ?? status;
}

const SVG = '<svg width="0" height="0" style="position:absolute" aria-hidden="true">'
  + '<symbol id="i-quote" viewBox="0 0 16 16"><path d="M4 4h4v4c0 2-1 3-3 3M9 4h4v4c0 2-1 3-3 3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></symbol>'
  + '<symbol id="i-paraphrase" viewBox="0 0 16 16"><path d="M3 8h7M8 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></symbol>'
  + '<symbol id="i-infer" viewBox="0 0 16 16"><path d="M8 2l1.6 4.4L14 8l-4.4 1.6L8 14 6.4 9.6 2 8l4.4-1.6z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/></symbol>'
  + '</svg>';

export function renderPage(results, { title = '证据与三色引用 · 预览' } = {}) {
  const s = summarize(results);
  return '<!doctype html>'
    + '<html lang="zh-CN"><head><meta charset="utf-8">'
    + '<meta name="viewport" content="width=device-width, initial-scale=1">'
    + '<title>' + esc(title) + '</title>'
    + '<link rel="stylesheet" href="../../p0-05-card/tokens.css">'
    + '<link rel="stylesheet" href="preview.css"></head><body>'
    + '<header class="bar"><b>鹿蕊</b><span>证据与三色引用</span>'
    + '<span class="stat">有据 ' + s.grounded + ' / 弱 ' + s.weak + ' / 推断 ' + s.inferred
    + ' / 无源 ' + s.ungrounded + ' / 对不上 ' + s.broken + '　直接引用率 ' + s.directRate + '%</span></header>'
    + '<main>' + results.map(renderClaim).join('') + appendix(results) + '</main>'
    + '<footer>固定文案：' + esc(NO_SOURCE_COPY) + '　·　推断永远不算证据</footer>'
    + SVG + '</body></html>';
}
