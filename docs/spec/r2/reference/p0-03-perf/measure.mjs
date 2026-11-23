// 量三件真能量的事：文件系统读、内核 API、以及「哪些测不了」。
// 不猜、不造数：内核没开就打印 NOT MEASURED 和命令，不写假数字。
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

const arg = (name, fallback) => {
  const i = process.argv.indexOf('--' + name);
  return i > -1 ? process.argv[i + 1] : fallback;
};
const vault = arg('vault', '/tmp/litmus-perf-vault');
const kernel = arg('kernel', 'http://127.0.0.1:7717');
const samples = Number(arg('samples', 200));

const pct = (arr, p) => {
  const s = [...arr].sort((a, b) => a - b);
  return +s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))].toFixed(2);
};

function scan(dir) {
  const out = [];
  for (const d of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, d.name);
    if (d.isDirectory()) out.push(...scan(p));
    else if (d.name.endsWith('.md')) out.push(p);
  }
  return out;
}

const t0 = process.hrtime.bigint();
const files = scan(join(vault, 'Notes'));
const scanMs = Number(process.hrtime.bigint() - t0) / 1e6;
const totalBytes = files.reduce((n, f) => n + statSync(f).size, 0);

const lat = [];
for (let i = 0; i < Math.min(samples, files.length); i++) {
  const f = files[(i * 7919) % files.length];
  const t = process.hrtime.bigint();
  readFileSync(f, 'utf8');
  lat.push(Number(process.hrtime.bigint() - t) / 1e6);
}

async function timeFetch(path) {
  const t = process.hrtime.bigint();
  const res = await fetch(kernel + path, { signal: AbortSignal.timeout(4000) });
  await res.arrayBuffer();
  return { ms: Number(process.hrtime.bigint() - t) / 1e6, status: res.status };
}

const report = {
  vault,
  files: files.length,
  mb: +(totalBytes / 1048576).toFixed(1),
  scan_ms: +scanMs.toFixed(1),
  read_ms: { p50: pct(lat, 50), p95: pct(lat, 95) },
  kernel: {},
  not_measured: [
    '冷启动到可交互（需 App 运行：kill; time open -a Litmus）',
    '⌥Space 到可输入（需 quickcapture 运行，读 .litmus/local/ops/quickcapture.log）',
    '本地搜索 1 万篇 p95（需内核 /search/hybrid）',
    '空闲 CPU / 内存（需 ps -o %cpu,rss -p <pid>）',
  ],
};

// 端点名以 p1-04 的核实报告为准：没有 /docs/tree，实际是 /browse/tree。
for (const path of ['/health/status', '/browse/tree', '/search/hybrid?q=' + encodeURIComponent('判断')]) {
  try {
    const r = await timeFetch(path);
    report.kernel[path] = r;
  } catch (e) {
    report.kernel[path] = { error: String(e.message || e) };
  }
}

console.log(JSON.stringify(report, null, 2));
