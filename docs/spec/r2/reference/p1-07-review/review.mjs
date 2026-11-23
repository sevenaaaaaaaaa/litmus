// P1-07 · 「像我」盲评机制的纯函数实现。
// 唯一要回答的问题：这段文字像不像本人，能不能不靠作者自夸来证明。
//
// 规则（与 18-like-me-review.md 一致）：
//   两位评审独立打分（1–5），分差 ≤1 取均值；
//   分差 ≥2 触发第三人盲评，取三人中位数；
//   三人极差仍 ≥2 → 无法判定，该段回炉，不计入通过数；
//   10 段里 ≥7 段达到 4 分、且没有待裁决/无法判定段 → 通过。
//
// 颜色、字体、模型都不在这里；这里只有分数与判定。

import { pathToFileURL } from 'node:url';

export const SECTIONS = Object.freeze([
  { id: 'judgment',   title: '判断',   tests: '面对一个模糊选择时，先看什么、怎么下判断', anchors: '判断 / 价值' },
  { id: 'taste',      title: '审美',   tests: '看到一版设计或一段文字的第一反应',           anchors: '审美 / 偏好' },
  { id: 'boundary',   title: '边界',   tests: '什么情况下会拒绝，拒绝时怎么说',             anchors: '边界 / 不做' },
  { id: 'counter',    title: '反例',   tests: '主动说出自己可能错在哪',                     anchors: '反例 / 不确定' },
  { id: 'workstyle',  title: '工作方式', tests: '一天怎么开始，卡住时怎么办',                 anchors: '工作方式 / 节奏' },
  { id: 'relation',   title: '关系',   tests: '怎么对待合作者或朋友的不同意见',             anchors: '关系 / 协作' },
  { id: 'language',   title: '句法',   tests: '常用句式、断句、口头禅（不是内容）',          anchors: '声音 / 语言' },
  { id: 'memory',     title: '记忆',   tests: '会记住什么细节，为什么会记住',               anchors: '记忆 / 细节' },
  { id: 'time',       title: '时间观', tests: '长期与短期怎么权衡',                         anchors: '时间 / 取舍' },
  { id: 'unknown',    title: '未知',   tests: '明确说“我还不知道”的领域，以及怎么面对',     anchors: '未知 / 边界' },
]);

const MIN = 1;
const MAX = 5;

const isScore = (x) => Number.isInteger(x) && x >= MIN && x <= MAX;

function assertScore(x, label) {
  if (!isScore(x)) throw new Error(label + ' 必须是 ' + MIN + '–' + MAX + ' 的整数，收到 ' + JSON.stringify(x));
}

/** 单段判定：返回 {a,b,c,gap,spread,score,status}。 */
export function scoreSegment(a, b, c = null) {
  assertScore(a, '评审 A 的分数');
  assertScore(b, '评审 B 的分数');
  const gap = Math.abs(a - b);
  if (gap <= 1) {
    return { a, b, c: null, gap, spread: null, score: (a + b) / 2, status: 'agreed' };
  }
  if (c === null || c === undefined) {
    return { a, b, c: null, gap, spread: null, score: null, status: 'needs-arbitration' };
  }
  assertScore(c, '第三人裁决分');
  const vals = [a, b, c].slice().sort((x, y) => x - y);
  const spread = vals[2] - vals[0];
  // 两人差 2 分时，第三人落在中间就可裁决；第三人把极差拉到 3 分以上，
  // 说明这段的“像不像”本身没有共识，回炉，不用分数硬凑。
  if (spread >= 3) {
    return { a, b, c, gap, spread, score: null, status: 'undecidable' };
  }
  return { a, b, c, gap, spread, score: vals[1], status: 'arbitrated' };
}

/** 二次加权 Cohen's κ（1–5 分，k=5）。全同分且完全一致时返回 1。 */
export function weightedKappa(pairs) {
  if (!Array.isArray(pairs) || pairs.length === 0) throw new Error('没有可算的评分配对');
  const k = MAX;
  const n = pairs.length;
  const observed = Array.from({ length: k }, () => Array(k).fill(0));
  const row = Array(k).fill(0);
  const col = Array(k).fill(0);
  for (const { a, b } of pairs) {
    assertScore(a, 'κ 的评审 A 分数');
    assertScore(b, 'κ 的评审 B 分数');
    observed[a - 1][b - 1] += 1;
    row[a - 1] += 1;
    col[b - 1] += 1;
  }
  const weight = (i, j) => 1 - ((i - j) ** 2) / ((k - 1) ** 2);
  let po = 0;
  let pe = 0;
  for (let i = 0; i < k; i += 1) {
    for (let j = 0; j < k; j += 1) {
      po += weight(i, j) * (observed[i][j] / n);
      pe += weight(i, j) * ((row[i] * col[j]) / (n * n));
    }
  }
  if (Math.abs(1 - pe) < 1e-12) return 1;
  return (po - pe) / (1 - pe);
}

/** 整卷判定：返回逐段结果、通过数、κ、判定。 */
export function judge(ratings) {
  if (!Array.isArray(ratings)) throw new Error('ratings 必须是数组');
  if (ratings.length !== SECTIONS.length) {
    throw new Error('必须恰好 ' + SECTIONS.length + ' 段，收到 ' + ratings.length);
  }
  const seen = new Set();
  const rows = ratings.map((r) => {
    if (!r || typeof r.id !== 'string') throw new Error('每段必须有 id');
    if (!SECTIONS.some((s) => s.id === r.id)) throw new Error('未知段 id：' + r.id);
    if (seen.has(r.id)) throw new Error('重复段 id：' + r.id);
    seen.add(r.id);
    return { id: r.id, ...scoreSegment(r.a, r.b, r.c) };
  });
  for (const s of SECTIONS) {
    if (!seen.has(s.id)) throw new Error('缺段：' + s.id);
  }
  const good = rows.filter((r) => r.score !== null && r.score >= 4).length;
  const undecidable = rows.filter((r) => r.status === 'undecidable').length;
  const pending = rows.filter((r) => r.status === 'needs-arbitration').length;
  const kappa = weightedKappa(rows.map((r) => ({ a: r.a, b: r.b })));

  let verdict;
  if (pending > 0) verdict = 'needs-arbitration';
  else if (undecidable > 0) verdict = good >= 4 && undecidable <= 2 ? 'revise' : 'fail';
  else if (good >= 7) verdict = 'pass';
  else if (good >= 4) verdict = 'revise';
  else verdict = 'fail';

  return { rows, good, undecidable, pending, kappa, verdict };
}

export function formatReport(result) {
  const lines = [];
  lines.push('P1-07 盲评 · ' + SECTIONS.length + ' 段');
  lines.push('段'.padEnd(12) + 'A  B  裁决  得分  状态');
  for (const r of result.rows) {
    lines.push(
      r.id.padEnd(12) +
        String(r.a).padEnd(2) +
        String(r.b).padEnd(3) +
        (r.c === null ? '-' : String(r.c)).padEnd(6) +
        (r.score === null ? '—' : r.score.toFixed(1)).padEnd(6) +
        r.status
    );
  }
  lines.push('');
  lines.push('达到 4 分：' + result.good + '/' + SECTIONS.length);
  lines.push('待裁决：' + result.pending + ' · 无法判定：' + result.undecidable);
  lines.push('二次加权 κ：' + result.kappa.toFixed(3));
  lines.push('判定：' + result.verdict);
  return lines.join('\n');
}

// CLI：node review.mjs --in ratings.json [--json]
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { readFileSync } = await import('node:fs');
  const args = process.argv.slice(2);
  const at = args.indexOf('--in');
  if (at < 0 || !args[at + 1]) {
    console.error('用法：node review.mjs --in ratings.json [--json]');
    process.exit(2);
  }
  const ratings = JSON.parse(readFileSync(args[at + 1], 'utf8'));
  const result = judge(ratings);
  if (args.includes('--json')) console.log(JSON.stringify(result, null, 2));
  else console.log(formatReport(result));
  process.exit(result.verdict === 'pass' ? 0 : 1);
}
