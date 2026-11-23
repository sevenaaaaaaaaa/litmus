// 端点核实：把"规划里说要复用的接口"变成一次可重跑的证据检查。
// 只做一件事：在已安装的二进制里找路由字符串，报告在/不在。
// 找得到 ≠ 能用；找不到 = 一定不能用。
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';

const here = new URL('.', import.meta.url);

export function scan(buffer, needle) {
  let count = 0;
  let at = buffer.indexOf(needle);
  while (at >= 0) {
    count++;
    at = buffer.indexOf(needle, at + 1);
  }
  return count;
}

export function verifyRoutes(routes, buffer) {
  return routes.map((r) => {
    const needle = Buffer.from(r.route, 'utf8');
    const count = scan(buffer, needle);
    const ok = r.status === 'missing' ? count === 0 : count > 0;
    return { ...r, count, ok };
  });
}

export function report(rows, binary) {
  const bad = rows.filter((r) => !r.ok);
  const lines = [
    '# 端点核实报告（由 verify-endpoints.mjs 生成）',
    '',
    '- 二进制：' + binary.path,
    '- 大小：' + binary.bytes + ' bytes',
    '- SHA-256：' + binary.sha256,
    '- 扫描时间：' + new Date().toISOString(),
    '',
    '| 端点 | 用途 | 状态 | 命中次数 | 判定 |',
    '|---|---|---|---|---|',
    ...rows.map((r) => '| \`' + r.route + '\` | ' + r.purpose + ' | ' + r.status + ' | ' + r.count + ' | ' + (r.ok ? 'OK' : 'FAIL') + ' |'),
    '',
    bad.length ? '**' + bad.length + ' 条不符合预期**' : '全部符合预期。',
    '',
    '> 命中 ≠ 能用：这只证明路由字符串在二进制里，方法/入参/返回仍需真实请求核实。',
  ];
  return lines.join(String.fromCharCode(10));
}

const isMain = process.argv[1] && process.argv[1].endsWith('verify-endpoints.mjs');
if (isMain) {
  const cfg = JSON.parse(readFileSync(new URL('endpoints.json', here), 'utf8'));
  if (!existsSync(cfg.binary.path)) {
    console.log('SKIP 二进制不存在：' + cfg.binary.path);
    process.exit(0);
  }
  const buf = readFileSync(cfg.binary.path);
  const rows = verifyRoutes(cfg.routes, buf);
  const md = report(rows, cfg.binary);
  mkdirSync(new URL('dist/', here), { recursive: true });
  writeFileSync(new URL('dist/endpoints-report.md', here), md);
  for (const r of rows) console.log((r.ok ? 'OK   ' : 'FAIL ') + r.route.padEnd(20, ' ') + ' x' + r.count);
  console.log('---');
  console.log('报告：p1-04-advance/dist/endpoints-report.md');
  process.exit(rows.every((r) => r.ok) ? 0 : 1);
}
