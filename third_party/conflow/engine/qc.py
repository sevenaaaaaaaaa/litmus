"""ConFlow 服务端 · 幻灯片质量基线：产出后自动质检每页，溢出/过载标出。

两种模式（结果结构一致，mode 字段如实标注）：
  render     无头 Chrome 打开 slides.html?qc=1，读引擎自检的每页缩放比与溢出（真实布局）
  heuristic  无浏览器可用时，按内容量预算（文本/要点/代码/图）静态估算（保守阈值）

判定（render）：ratio ≥ .98 通过；.8–.98 警告（整体被压缩）；< .8 失败；无 .body 页溢出即失败。
结果：{"mode", "pages": [{"page","status","ratio","reason"}], "ok","warn","fail"} —— 质检不拦截任务，只在详情标黄。
"""
from __future__ import annotations

import json
import os
import pathlib
import re
import shutil
import subprocess

_CHROME_CANDIDATES = ("chrome-headless-shell", "chromium", "chromium-browser", "google-chrome")


def find_chrome() -> str | None:
    """env CONFLOW_CHROME > PATH > playwright 缓存（mac/linux 常见位置）。"""
    env = os.environ.get("CONFLOW_CHROME")
    if env and pathlib.Path(env).exists():
        return env
    for name in _CHROME_CANDIDATES:
        p = shutil.which(name)
        if p:
            return p
    for cache in (pathlib.Path.home() / "Library/Caches/ms-playwright",
                  pathlib.Path.home() / ".cache/ms-playwright"):
        if cache.is_dir():
            hits = sorted(cache.glob("*/chrome-headless-shell*/chrome-headless-shell"))
            if hits:
                return str(hits[-1])
    return None


def _classify(item: dict) -> dict:
    ratio = float(item.get("ratio", 1))
    overflow = bool(item.get("overflow"))
    if overflow or ratio < 0.8:
        status = "fail"
        reason = "页面溢出" if overflow else f"内容被整体压缩到 {round(ratio*100)}%"
    elif ratio < 0.98:
        status, reason = "warn", f"内容被整体压缩到 {round(ratio*100)}%"
    else:
        status, reason = "ok", ""
    return {"page": item.get("page"), "status": status,
            "ratio": round(ratio, 3), "reason": reason}


def render_qc(deck: pathlib.Path) -> dict:
    chrome = find_chrome()
    url = f"file://{deck.resolve()}?qc=1"
    r = subprocess.run([chrome, "--headless", "--disable-gpu", "--no-sandbox",
                        "--virtual-time-budget=4000", "--dump-dom", url],
                       capture_output=True, text=True, timeout=60)
    m = re.search(r'<script type="application/json" id="qc-report">(.+?)</script>',
                  r.stdout, re.S)
    if not m:
        raise RuntimeError("引擎未输出质检报告（deck 由旧版引擎生成？）")
    pages = [_classify(it) for it in json.loads(m.group(1))]
    return _pack("render", pages)


# ------------------------------------------------------------- heuristic ----

# 预算按真实成品校准（四套 demo 最重页 362 字符 / 4 要点）——只拦真正的超载页
_BUDGET = {"text_fail": 900, "text_warn": 600, "li_warn": 10, "pre_warn": 2400, "fig_warn": 6}


def heuristic_qc(deck: pathlib.Path) -> dict:
    html = deck.read_text(encoding="utf-8")
    start = html.find("<!-- ▼▼")
    body = html[start:] if start != -1 else html
    body = body.split('<div id="progress">')[0]
    pages = []
    for i, raw in enumerate(re.split(r'<section class="slide', body)[1:], 1):
        pg = raw.split("</section>")[0]
        text = re.sub(r"\s+", "", re.sub(r"<[^>]+>", "", pg))
        lis = len(re.findall(r"<li", pg))
        pre = "".join(re.findall(r"<pre[^>]*>(.*?)</pre>", pg, re.S))
        figs = len(re.findall(r"<figure", pg))
        reasons = []
        if len(text) > _BUDGET["text_fail"]:
            status = "fail"
            reasons.append(f"文本量 {len(text)} 字符，超出单页承载")
        else:
            if len(text) > _BUDGET["text_warn"]:
                reasons.append(f"文本量 {len(text)} 字符偏高")
            if lis > _BUDGET["li_warn"]:
                reasons.append(f"要点 {lis} 条偏多")
            if len(re.sub(r"\s+", "", pre)) > _BUDGET["pre_warn"]:
                reasons.append("代码块过长")
            if figs > _BUDGET["fig_warn"]:
                reasons.append(f"图片 {figs} 张偏多")
            status = "warn" if reasons else "ok"
        pages.append({"page": i, "status": status, "ratio": 1,
                      "reason": "；".join(reasons)})
    return _pack("heuristic", pages)


def _pack(mode: str, pages: list[dict]) -> dict:
    return {"mode": mode, "pages": pages,
            "ok": sum(1 for p in pages if p["status"] == "ok"),
            "warn": sum(1 for p in pages if p["status"] == "warn"),
            "fail": sum(1 for p in pages if p["status"] == "fail")}


def check_deck(deck: pathlib.Path) -> dict:
    """质检入口：有浏览器走真实渲染，失败或缺失退回启发式；deck 不存在则跳过。"""
    if not deck.exists():
        return {"mode": "skipped", "pages": [], "ok": 0, "warn": 0, "fail": 0}
    if find_chrome():
        try:
            return render_qc(deck)
        except Exception:
            pass  # 渲染环境问题不阻塞任务，退启发式
    return heuristic_qc(deck)
