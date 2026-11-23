// 校验埋点字典与指标定义是否对得上。
// 核心检查：每个指标引用的每个事件、每个字段都必须真的存在。
// 这类错最容易发生：仪表盘上有个数字，但从来没人上报它。
import { readFileSync } from 'node:fs';

const TYPES = new Set(['enum', 'int', 'bool', 'string']);

export function checkEvents(doc) {
  const errors = [];
  const warnings = [];
  if (!Array.isArray(doc.events) || doc.events.length === 0) errors.push('events 为空');
  const seen = new Set();
  for (const e of doc.events ?? []) {
    if (!e.name) { errors.push('事件缺 name'); continue; }
    if (seen.has(e.name)) errors.push('事件重名: ' + e.name);
    seen.add(e.name);
    if (!doc.piiLevels.includes(e.pii)) errors.push(e.name + ' 的 pii 档位非法: ' + e.pii);
    const role = e.role ?? 'metric';
    if (!['metric', 'diagnostic'].includes(role)) errors.push(e.name + ' 的 role 非法: ' + role);
    const props = e.properties ?? {};
    if (Object.keys(props).length === 0) errors.push(e.name + ' 没有任何属性');
    for (const [k, p] of Object.entries(props)) {
      if (!TYPES.has(p.type)) errors.push(e.name + '.' + k + ' 类型非法: ' + p.type);
      if (p.type === 'enum' && (!p.values || p.values.length < 2)) errors.push(e.name + '.' + k + ' 枚举少于两个值');
      if (p.type === 'int' && (p.min === undefined || p.max === undefined)) warnings.push(e.name + '.' + k + ' 没有上下界');
    }
  }
  return { errors, warnings, names: [...seen] };
}

export function checkMetrics(doc, eventNames) {
  const errors = [];
  const warnings = [];
  const ids = new Set();
  for (const m of doc.metrics ?? []) {
    if (ids.has(m.id)) errors.push('指标重名: ' + m.id);
    ids.add(m.id);
    for (const k of ['name', 'question', 'unit', 'formula', 'baseline', 'direction']) {
      if (!m[k]) errors.push(m.id + ' 缺 ' + k);
    }
    if (m.target90 === undefined || m.target12m === undefined) errors.push(m.id + ' 缺目标值');
    if (!['up', 'down'].includes(m.direction)) errors.push(m.id + ' direction 只能是 up/down');
    for (const ev of m.events ?? []) {
      if (!eventNames.includes(ev)) errors.push(m.id + ' 引用了不存在的事件: ' + ev);
    }
    for (const need of m.needs ?? []) {
      const ev = eventNames.find((n) => need.startsWith(n + '.'));
      if (!ev) { errors.push(m.id + ' 的字段引用找不到事件: ' + need); continue; }
      // 字段存在性在调用方用完整 schema 复查
    }
    if ((m.events ?? []).length === 0 && !m.note) {
      errors.push(m.id + ' 没有任何事件支撑，也没有说明为什么是人工指标');
    }
  }
  return { errors, warnings };
}

export function validate(eventsDoc, metricsDoc) {
  const ev = checkEvents(eventsDoc);
  const errors = [...ev.errors];
  const warnings = [...ev.warnings];

  const byName = new Map((eventsDoc.events ?? []).map((e) => [e.name, e]));
  // needs 的字段级检查（事件名本身带点，取最长前缀匹配）
  for (const m of metricsDoc.metrics ?? []) {
    for (const need of m.needs ?? []) {
      const evName = [...byName.keys()].filter((n) => need.startsWith(n + '.')).sort((a, b) => b.length - a.length)[0];
      if (!evName) continue;
      const field = need.slice(evName.length + 1);
      if (!byName.get(evName).properties?.[field]) {
        errors.push(m.id + ' 引用的字段不存在: ' + need);
      }
    }
  }
  const mt = checkMetrics(metricsDoc, [...byName.keys()]);
  errors.push(...mt.errors);
  warnings.push(...mt.warnings);

  // 没有被任何指标用到的事件：不是错，但每一份埋点都有成本。
  const used = new Set((metricsDoc.metrics ?? []).flatMap((m) => m.events ?? []));
  const orphan = [...byName.entries()]
    .filter(([n, e]) => (e.role ?? 'metric') === 'metric' && !used.has(n))
    .map(([n]) => n);
  if (orphan.length) warnings.push('role=metric 却没有指标消费的事件: ' + orphan.join(', '));
  const diag = [...byName.entries()].filter(([n, e]) => (e.role ?? 'metric') === 'diagnostic').map(([n]) => n);
  if (diag.length) warnings.push('diagnostic 事件（只排查用）: ' + diag.join(', '));

  return { ok: errors.length === 0, errors, warnings };
}

if (process.argv[1]?.endsWith('validate.mjs')) {
  const evDoc = JSON.parse(readFileSync(new URL('../p0-01-events/events.schema.json', import.meta.url), 'utf8'));
  const mtDoc = JSON.parse(readFileSync(new URL('./metrics.json', import.meta.url), 'utf8'));
  const r = validate(evDoc, mtDoc);
  for (const w of r.warnings) console.log('WARN  ' + w);
  for (const e of r.errors) console.log('ERROR ' + e);
  console.log(r.ok ? 'OK ' + mtDoc.metrics.length + ' 个指标 / ' + evDoc.events.length + ' 个事件全部对得上' : 'FAILED');
  process.exit(r.ok ? 0 : 1);
}
