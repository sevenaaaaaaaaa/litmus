// Litmus Clipper · 页面侧自动授权（≥0.3.0）
// 打开已登录的 Litmus 页面 → 向页面发起握手 → 应用内确认一次 → 地址与令牌自动入库，无需手动复制。
const api = globalThis.browser ?? globalThis.chrome;

window.addEventListener('message', async (e) => {
  if (e.origin !== location.origin || !e.data) return;
  if (e.data.type === 'litmus-auth-response' && e.data.token) {
    await api.storage.local.set({ token: String(e.data.token), baseUrl: String(e.data.base || '') });
    try { api.runtime.sendMessage({ type: 'litmus:auth-ok' }); } catch (err) { /* 后台休眠时忽略 */ }
  }
});

(async () => {
  try {
    const d = await api.storage.local.get(['token']);
    if (d.token) return;                       // 已配置：不打扰
    if (sessionStorage.getItem('litmus-auth-tried')) return; // 每个标签页会话只握手一次
    sessionStorage.setItem('litmus-auth-tried', '1');
    setTimeout(() => { try { window.postMessage({ type: 'litmus-auth-request' }, location.origin); } catch (err) {} }, 1800);
  } catch (err) { /* storage 不可用时静默 */ }
})();
