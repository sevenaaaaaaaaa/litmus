// 卡的状态机：五个状态的唯一真相。
// 只读原型与将来的正式实现都引用这一份，避免 UI 和文案各写一套。
// 规则：每个状态必须有 文字 + 图标 + 主动作；颜色只是第三信道。

export const ORDER = ['seed', 'grounded', 'tension', 'settled', 'shipped'];

export const STATES = {
  seed: {
    key: 'seed', label: '种子', icon: 'dotted-circle', tone: 'state-seed',
    meaning: '刚落下，还没有可点回的来源',
    action: '找证据',
    coverage: 0.12,
    quick: '先给它一条能点回去的来源',
  },
  grounded: {
    key: 'grounded', label: '有据', icon: 'circle-check', tone: 'state-grounded',
    meaning: '至少有一条可点回的来源',
    action: '找反例',
    coverage: 0.45,
    quick: '主动找一条反对它的证据',
  },
  tension: {
    key: 'tension', label: '有张力', icon: 'triangle-alert', tone: 'state-tension',
    meaning: '与另一条证据或判断冲突，尚未决定',
    action: '做决定',
    coverage: 0.62,
    quick: '在「改卡 / 改来源 / 并存」里选一个',
  },
  settled: {
    key: 'settled', label: '已定', icon: 'square', tone: 'state-settled',
    meaning: '用户确认过，暂时稳定',
    action: '成稿',
    coverage: 0.82,
    quick: '把它变成一段能拿出去的文字',
  },
  shipped: {
    key: 'shipped', label: '已发布', icon: 'badge-check', tone: 'state-shipped',
    meaning: '已经作为页面或作品发出去了',
    action: '回流 / 更新',
    coverage: 1,
    quick: '等读者反馈回来，决定改还是留',
  },
};

const EVENTS = {
  'source.added': (s) => (s === 'seed' ? 'grounded' : null),
  'source.conflict': (s) => (s === 'grounded' || s === 'settled' ? 'tension' : null),
  decide: (s) => (s === 'tension' ? 'settled' : null),
  publish: (s) => (s === 'settled' || s === 'shipped' ? 'shipped' : null),
  unpublish: (s) => (s === 'shipped' ? 'settled' : null),
  reset: () => 'seed',
};

// 返回 {ok, state, reason}。拒绝要给出人能读的理由，不静默。
export function reduce(state, event) {
  if (!STATES[state]) throw new Error('unknown state: ' + state);
  const fn = EVENTS[event];
  if (!fn) throw new Error('unknown event: ' + event);
  const next = fn(state);
  if (!next) {
    return { ok: false, state, reason: reasonFor(state, event) };
  }
  return { ok: true, state: next, reason: null };
}

function reasonFor(state, event) {
  if (event === 'publish' && state === 'grounded') return '还没做决定：先在有张力里选完，才能发布';
  if (event === 'publish' && state === 'seed') return '没有来源的卡不发：先找证据';
  if (event === 'decide' && state !== 'tension') return '只有有张力的卡需要做决定';
  if (event === 'source.added' && state !== 'seed') return '已经有来源了，这一步不用重做';
  if (event === 'unpublish' && state !== 'shipped') return '这张卡还没发布过';
  return state + ' 不接受 ' + event;
}

export const activeAction = (state) => STATES[state].action;
export const coverage = (state) => STATES[state].coverage;

// 「3 秒三问」：我在哪 / 这张卡什么状态 / 下一步是什么
export function quickCheck(state, ctx = {}) {
  const s = STATES[state];
  if (!s) throw new Error('unknown state: ' + state);
  return {
    where: ctx.space ? '空间 · ' + ctx.space : '空间 · 未命名',
    state: s.label + '（' + s.meaning + '）',
    next: s.action + '：' + s.quick,
    a11y: s.label + '，' + s.action, // 屏幕阅读器读一句，不读颜色
  };
}

// 颜色只是第三信道：任何状态都必须有文字与图标，否则 UI 视为不合规。
export function assertAccessible(state) {
  const s = STATES[state];
  const missing = [];
  if (!s.label) missing.push('label');
  if (!s.icon) missing.push('icon');
  if (!s.action) missing.push('action');
  return { ok: missing.length === 0, missing };
}
