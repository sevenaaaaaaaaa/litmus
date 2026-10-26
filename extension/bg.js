// Litmus Clipper · 后台（Chrome/Edge + Firefox 共用）
// 用 globalThis 兼容 browser.* 与 chrome.*
const api = globalThis.browser ?? globalThis.chrome;

/** 规范化地址：补 /litmus 前缀（若用户只填了域名）、去尾斜杠 */
function normalizeBase(raw) {
  let base = (raw || "").trim().replace(/\/+$/, "");
  if (!base) return "";
  // nownexts.com 这类主域下 Litmus 挂在 /litmus；若用户填了裸域名则自动补
  try {
    const u = new URL(base);
    if (u.hostname === "nownexts.com" && u.pathname === "") {
      base = u.origin + "/litmus";
    }
  } catch (e) { /* 非法 URL 交给 fetch 报错 */ }
  return base;
}

function notify(title, message) {
  try {
    api.notifications.create({
      type: "basic",
      iconUrl: api.runtime.getURL("icon48.png"),   // 必须是绝对 URL（Firefox 强制）
      title,
      message: String(message).slice(0, 300),
    });
  } catch (e) { /* 没有通知权限时忽略 */ }
}

async function clipPage(info, tab) {
  const cfg = await api.storage.local.get(["baseUrl", "token"]);
  const base = normalizeBase(cfg.baseUrl);
  if (!base || !cfg.token) {
    notify("鹿蕊未配置", "打开已登录的 Litmus 页面可一键自动授权；或点击扩展图标手动填写");
    return;
  }
  let html = "";
  try {
    html = await api.tabs.sendMessage(tab.id, { type: "litmus:html" });
  } catch (e) {
    // content script 未注入（如 chrome:// 页面）→ 用 scripting 兜底
    try {
      const res = await api.scripting.executeScript({
        target: { tabId: tab.id },
        func: () => document.documentElement.outerHTML,
      });
      html = res?.[0]?.result || "";
    } catch (e2) {
      notify("采集失败", "无法读取页面内容：" + e2.message);
      return;
    }
  }
  try {
    const resp = await fetch(`${base}/ingest/web`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${cfg.token}` },
      body: JSON.stringify({
        url: tab.url,
        title: tab.title,
        selection: info.selectionText || "",
        html: String(html).slice(0, 500000),
      }),
    });
    if (resp.status === 401) { notify("采集失败", "Token 无效，请在扩展里更新"); return; }
    const data = await resp.json().catch(() => ({}));
    if (resp.ok && data.rel) notify("✓ 已采集到鹿蕊", data.title || tab.title);
    else notify("采集失败", `HTTP ${resp.status} ${data.error || ""}`);
  } catch (e) {
    notify("网络错误", `${base} 不可达：${e.message}`);
  }
}

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.create({ id: "litmus-clip", title: "采集到鹿蕊", contexts: ["page", "selection"] });
});

// content.js 自动授权成功 → 气泡告知一次
api.runtime.onMessage.addListener((msg) => {
  if (msg && msg.type === "litmus:auth-ok") {
    notify("Litmus 已自动授权", "地址与令牌已就绪，右键任意网页即可采集");
  }
});

// 用非 async 包装，避免 Firefox "Promised response went out of scope" 警告
api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== "litmus-clip") return;
  clipPage(info, tab);
});
