import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  FileBackend, EndpointBackend, selectBackend, checkContract, BackendContractError, ENDPOINT_NEEDS, BACKEND_METHODS,
} from './backends.mjs';
import { makeRun, attachCandidate, evaluate, commit as rawCommit, snapshot, diffSnapshots } from '../p1-04-advance/advance.mjs';

const NL = String.fromCharCode(10);
const SOURCES = { 'a-1': { path: 'Notes/写作/a.md', text: '判断不是结论，是被反复使用过的取舍。' } };
const resolve = (a) => SOURCES[a] ?? null;

function vault() {
  const dir = mkdtempSync(join(tmpdir(), 'litmus-be-'));
  mkdirSync(join(dir, 'Notes/写作'), { recursive: true });
  writeFileSync(join(dir, 'Notes/写作/a.md'), '# A' + NL + NL + '判断不是结论，是被反复使用过的取舍。 ^a-1' + NL);
  return dir;
}
const run = (v) => {
  const r = makeRun(v, { space: '写作', anchors: ['a-1'], intent: 'evidence', text: '找证据' });
  attachCandidate(r, { text: '判断的价值在于被反复使用。', citations: [{ anchor: 'a-1', kind: 'direct', quote: '是被反复使用过的取舍' }] });
  evaluate(r, resolve);
  return r;
};

test('两个后端都满足同一份接口契约', () => {
  assert.deepEqual(checkContract(FileBackend('/tmp/x')).missing, []);
  assert.deepEqual(checkContract(EndpointBackend()).missing, []);
  assert.deepEqual(BACKEND_METHODS, ['id', 'capabilities', 'describe', 'commit', 'undo']);
});

test('文件后端：落盘与撤销都能真跑', () => {
  const v = vault();
  const be = FileBackend(v);
  const before = snapshot(join(v, 'Notes'));
  const r = run(v);
  const out = be.commit(r, resolve);
  assert.ok(existsSync(out.cardPath));
  assert.match(readFileSync(join(v, 'Notes/写作/a.md'), 'utf8'), /由 \^a-1 推进/);
  be.undo(out.writeKey);
  assert.deepEqual(diffSnapshots(before, snapshot(join(v, 'Notes'))), { added: [], removed: [], changed: [], same: true });
});

test('文件后端与底层实现结果一致（同一张卡，同一把钥匙）', () => {
  const v1 = vault(), v2 = vault();
  const be = FileBackend(v1);
  const r1 = run(v1), r2 = run(v2);
  const a = be.commit(r1, resolve);
  const b = rawCommit(r2, resolve);
  assert.equal(a.writeKey, b.writeKey);
  assert.equal(a.cardPath.replace(v1, ''), b.cardPath.replace(v2, ''));
});

test('端点后端：没有契约就显式失败，并列出缺的五件事', () => {
  const be = EndpointBackend();
  let err = null;
  try { be.commit(); } catch (e) { err = e; }
  assert.ok(err instanceof BackendContractError);
  assert.equal(err.code, 'endpoint-contract-unknown');
  assert.deepEqual(err.needs, ENDPOINT_NEEDS);
  assert.match(err.message, /别假装联调过/);
});

test('端点后端的撤销也显式失败（回收站语义未定）', () => {
  const be = EndpointBackend();
  assert.throws(() => be.undo('x'), (e) => e.code === 'endpoint-contract-unknown' && e.route === '/trash/restore');
});

test('能力声明必须诚实：端点后端没有任何一项能力为真', () => {
  const ep = EndpointBackend().capabilities;
  assert.deepEqual(ep, { card: false, link: false, undo: false, ledger: false });
  const file = FileBackend('/tmp/x').capabilities;
  assert.deepEqual(file, { card: true, link: true, undo: true, ledger: true });
});

test('选后端：默认文件；要求端点但没参数 → 抛错，绝不静默降级', () => {
  assert.equal(selectBackend({ vault: '/tmp/x' }).id, 'file');
  assert.equal(selectBackend({ vault: '/tmp/x', endpoint: { mode: 'file' } }).id, 'file');
  assert.throws(
    () => selectBackend({ vault: '/tmp/x', endpoint: { mode: 'endpoint' } }),
    (e) => e.code === 'missing-endpoint-params' && e.needs.length === 5,
  );
});

test('选后端：参数齐了才返回端点后端（能力仍为未验证）', () => {
  const params = Object.fromEntries(ENDPOINT_NEEDS.map((k) => [k, 'given']));
  const be = selectBackend({ vault: '/tmp/x', endpoint: { mode: 'endpoint', params } });
  assert.equal(be.id, 'endpoint');
  assert.equal(be.capabilities.card, false);
  assert.deepEqual(be.describe().missing, []);
});

test('契约检查能抓出缺方法的后端', () => {
  const broken = { id: 'x', capabilities: {}, describe: () => ({}), commit: () => {} };
  const r = checkContract(broken);
  assert.equal(r.ok, false);
  assert.deepEqual(r.missing, ['undo']);
});
