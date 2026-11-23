// 可替换后端（P1-05）
// 目的：把"落盘 + 连回 + 撤销"从具体实现里抽出来，让文件后端与端点后端共用同一份协议。
// 现在：文件后端真的能跑；端点后端只有一个显式失败的空壳——因为 /agent/apply 的入参出参还不知道。
// 关键纪律：拿不到参数时**不许静默降级到文件后端**，否则联调会以为自己在测端点。
import { commit as fileCommit, undo as fileUndo, cardPathFor } from '../p1-04-advance/advance.mjs';

export const BACKEND_METHODS = ['id', 'capabilities', 'describe', 'commit', 'undo'];

export class BackendContractError extends Error {
  constructor({ code, route, needs, message }) {
    super(message);
    this.name = 'BackendContractError';
    this.code = code;
    this.route = route;
    this.needs = needs;
  }
}

export function checkContract(backend) {
  const missing = BACKEND_METHODS.filter((m) => backend[m] === undefined);
  return { ok: missing.length === 0, missing, id: backend.id ?? null };
}

// ── 文件后端：直接复用 P1-04 的文件层实现，不重写一套 ──
export function FileBackend(vault) {
  return {
    id: 'file',
    capabilities: { card: true, link: true, undo: true, ledger: true },
    describe: () => ({
      writesTo: 'Notes/<空间>/advance-<writeKey>.md',
      linkBack: '在来源卡末尾追加一行 wikilink',
      undo: '删卡 + 从备份还原来源卡（逐字节）',
      verified: true,
    }),
    commit: (run, resolve) => fileCommit(run, resolve),
    undo: (writeKey) => fileUndo(vault, writeKey),
    cardPathFor: (run) => cardPathFor(run),
  };
}

// ── 端点后端：接口在，实现不在 ──
// 需要补齐的五件事写在 endpoint-contract.json 里；补齐之前任何调用都会显式失败。
export const ENDPOINT_NEEDS = ['method', 'pathParams', 'requestBody', 'responseShape', 'errorCodes'];

export function EndpointBackend(config = {}) {
  const route = config.route ?? '/agent/apply';
  const missing = ENDPOINT_NEEDS.filter((k) => !config.params?.[k]);
  return {
    id: 'endpoint',
    capabilities: { card: false, link: false, undo: false, ledger: false },
    describe: () => ({
      writesTo: route,
      linkBack: '未知（可能由内核负责）',
      undo: '未知（可能走 /trash/restore）',
      verified: false,
      missing,
    }),
    commit() {
      throw new BackendContractError({
        code: 'endpoint-contract-unknown',
        route,
        needs: missing.length ? missing : ENDPOINT_NEEDS,
        message: '端点后端还没有契约：' + (missing.length ? missing.join(' / ') : '参数未验证') + '。先用文件后端跑通，别假装联调过。',
      });
    },
    undo() {
      throw new BackendContractError({
        code: 'endpoint-contract-unknown',
        route: '/trash/restore',
        needs: ENDPOINT_NEEDS,
        message: '端点后端的撤销语义未确认：回收站要保留多久、是否可跨设备，都还没答案。',
      });
    },
  };
}

// 选择后端：显式要求端点却没给参数 → 直接失败，不降级。
export function selectBackend({ vault, endpoint } = {}) {
  if (endpoint && (endpoint.mode === 'endpoint' || endpoint.enabled === true)) {
    if (!endpoint.params || !Object.keys(endpoint.params).length) {
      throw new BackendContractError({
        code: 'missing-endpoint-params',
        route: endpoint.route ?? '/agent/apply',
        needs: ENDPOINT_NEEDS,
        message: '要求走端点后端，但没有拿到参数：' + ENDPOINT_NEEDS.join(' / '),
      });
    }
    return EndpointBackend(endpoint);
  }
  return FileBackend(vault);
}
