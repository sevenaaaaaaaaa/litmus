// Mac 侧读队列的参考实现（与 QueueFile.swift 同一份协议）。
// 用途：在没有 iOS 真机时，用同一套规则验证「原子写 / 幂等 / 坏文件不吞」。
import { mkdirSync, writeFileSync, renameSync, readdirSync, readFileSync, unlinkSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';

export const idFor = (kind, text, date = new Date()) =>
  createHash('sha256').update([kind, text, date.toISOString().slice(0, 16)].join('|')).digest('hex').slice(0, 32);

export function openQueue(root) {
  const queueDir = join(root, 'queue');
  const tmpDir = join(root, 'queue/.tmp');
  mkdirSync(queueDir, { recursive: true });
  mkdirSync(tmpDir, { recursive: true });

  return {
    // 原子入队：写 .tmp → rename；同名（同一幂等键）直接返回 false。
    enqueue(item) {
      const name = item.createdAt.replaceAll(':', '-') + '-' + item.id + '.json';
      const final = join(queueDir, name);
      if (existsSync(final)) return false;
      const tmp = join(tmpDir, randomUUID() + '.json');
      writeFileSync(tmp, JSON.stringify(item));
      renameSync(tmp, final);
      return true;
    },

    // 读全部；坏文件跳过但不删除（留给人工/后续修复）。
    drain() {
      const items = readdirSync(queueDir)
        .filter((n) => n.endsWith('.json'))
        .flatMap((n) => {
          try { return [JSON.parse(readFileSync(join(queueDir, n), 'utf8'))]; }
          catch { return []; }
        });
      // 文件名排序会把同一毫秒的两条按哈希排；真正的顺序是 createdAt，id 只用来打破平手。
      return items.sort((a, b) => (a.createdAt < b.createdAt ? -1 : a.createdAt > b.createdAt ? 1 : a.id < b.id ? -1 : 1));
    },

    remove(id) {
      const hit = readdirSync(queueDir).find((n) => n.endsWith('-' + id + '.json'));
      if (hit) unlinkSync(join(queueDir, hit));
      return Boolean(hit);
    },

    dirs: { queueDir, tmpDir },
  };
}
