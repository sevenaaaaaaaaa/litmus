import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { verifyRoutes, report, scan } from './verify-endpoints.mjs';

const cfg = JSON.parse(readFileSync(new URL('./endpoints.json', import.meta.url), 'utf8'));
const hasBin = existsSync(cfg.binary.path);
const buf = hasBin ? readFileSync(cfg.binary.path) : null;
const rows = buf ? verifyRoutes(cfg.routes, buf) : [];

test('端点清单自洽：每条都有分组、用途、状态', () => {
  assert.ok(cfg.routes.length >= 10);
  for (const r of cfg.routes) {
    assert.ok(r.route.startsWith('/'), r.route);
    assert.ok(r.group && r.purpose && r.status, r.route + ' 字段不全');
  }
  assert.ok(cfg.gaps.length >= 3, '至少记录三条差距');
});

test('规划声称可复用的 write-result 不存在（差距成立）', () => {
  const w = cfg.routes.find((r) => r.route === '/write-result');
  assert.equal(w.status, 'missing');
  if (hasBin) assert.equal(scan(buf, Buffer.from('/write-result')), 0);
});

test('所有 route-verified 端点都在二进制里命中', { skip: !hasBin }, () => {
  const bad = rows.filter((r) => !r.ok);
  assert.deepEqual(bad.map((b) => b.route), []);
});

test('把不存在的路由标成 verified，检查会失败', { skip: !hasBin }, () => {
  const fake = [{ route: '/definitely-not-a-route', status: 'route-verified', purpose: 'x' }];
  assert.equal(verifyRoutes(fake, buf)[0].ok, false);
});

test('把存在的路由标成 missing，也会失败（宣称缺失也要有证据）', { skip: !hasBin }, () => {
  const fake = [{ route: '/chat', status: 'missing', purpose: 'x' }];
  assert.equal(verifyRoutes(fake, buf)[0].ok, false);
});

test('报告带二进制指纹，并写清“命中≠能用”', () => {
  const md = report(rows.length ? rows : [{ route: '/x', purpose: 'p', status: 'route-verified', count: 0, ok: true }], cfg.binary);
  assert.ok(md.includes(cfg.binary.sha256));
  assert.ok(md.includes('命中 ≠ 能用'));
});
