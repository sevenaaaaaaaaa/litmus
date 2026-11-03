"""ConFlow 服务端 · 批量与订阅：把频道 / 播放列表展开为任务队列。

expand() 用 yt-dlp --flat-playlist 只取元数据（不下载视频，秒级返回），
单视频 URL 展开结果即其自身，调用方以条目数区分单/批。
"""
from __future__ import annotations

import json
import re
import subprocess
import sys

# 这些形态按整列表展开（其余一律按单视频处理）
_BATCH_PATTERNS = (
    "playlist?list=", "&list=", "/playlist",
    "/channel/", "/@",
    "space.bilibili.com", "/favlist", "medialist",
)


def looks_batch(url: str) -> bool:
    u = url.lower()
    return any(p in u for p in _BATCH_PATTERNS)


def playlist_url(url: str) -> str:
    """watch?v=…&list=… → 纯播放列表 URL，避免 yt-dlp 落到单视频页。"""
    m = re.search(r"[?&]list=([\w-]+)", url)
    if m and "youtube" in url:
        return f"https://www.youtube.com/playlist?list={m.group(1)}"
    return url


def expand(url: str, limit: int = 20, proxy: str | None = None) -> list[dict]:
    """展开为 [{id, title, url}]；失败抛 RuntimeError（带 stderr 尾部）。"""
    limit = max(1, min(int(limit or 20), 200))
    cmd = [sys.executable, "-m", "yt_dlp", "--flat-playlist", "--dump-json",
           "--no-warnings", "--playlist-items", f"1:{limit}"]
    if proxy:
        cmd += ["--proxy", proxy]
    cmd.append(playlist_url(url))
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError("列表展开超时") from exc
    entries = []
    for line in (r.stdout or "").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except ValueError:
            continue
        vid = d.get("id")
        if not vid:
            continue
        e_url = d.get("url") or d.get("webpage_url") or ""
        if not e_url.startswith("http"):
            e_url = f"https://www.youtube.com/watch?v={vid}"
        entries.append({"id": vid, "title": (d.get("title") or vid)[:120], "url": e_url})
    if not entries:
        err = (r.stderr or "").strip().splitlines()
        raise RuntimeError("列表展开失败：" + (err[-1][:200] if err else "无条目返回"))
    return entries
