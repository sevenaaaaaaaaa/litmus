"""ConFlow 引擎 · LLM 客户端（与运行形态解耦）。

配置由 provider 提供（返回 {base_url, api_key, model, max_tokens, vision}）：
  服务端  server/llm.py 注入 store 热读 provider（server-data/config.json 优先）
  CLI     默认 provider：CONFLOW_LLM_* 环境变量 > ~/.conflow/config.json
  插件    engine.llm.set_provider(lambda: {...}) 程序化注入
"""
from __future__ import annotations

import base64
import json
import os
import pathlib
import urllib.request

_provider: callable | None = None
_ENV_KEYS = {"CONFLOW_LLM_API_KEY": "api_key", "CONFLOW_LLM_BASE_URL": "base_url",
             "CONFLOW_LLM_MODEL": "model", "CONFLOW_LLM_MAX_TOKENS": "max_tokens"}
_ENV_FALLBACK = {"CONFLOW_LLM_API_KEY": "V2HTML_LLM_API_KEY",
                 "CONFLOW_LLM_BASE_URL": "V2HTML_LLM_BASE_URL",
                 "CONFLOW_LLM_MODEL": "V2HTML_LLM_MODEL",
                 "CONFLOW_LLM_MAX_TOKENS": "V2HTML_LLM_MAX_TOKENS"}


def set_provider(fn: callable) -> None:
    """注入配置提供者：无参函数，返回 LLM 配置 dict。"""
    global _provider
    _provider = fn


def default_provider() -> dict:
    """CLI 默认配置：环境变量 > ~/.conflow/config.json。"""
    cfg: dict = {}
    conf = pathlib.Path.home() / ".conflow" / "config.json"
    if conf.exists():
        try:
            cfg = (json.loads(conf.read_text(encoding="utf-8")) or {}).get("llm") or {}
        except Exception:
            pass
    for new, key in _ENV_KEYS.items():
        v = os.environ.get(new) or os.environ.get(_ENV_FALLBACK[new])
        if v:
            cfg[key] = v
    return cfg


def cfg() -> dict:
    fn = _provider or default_provider
    c = fn() or {}
    c.setdefault("base_url", "https://open.bigmodel.cn/api/paas/v4")
    c.setdefault("model", "glm-4.7")
    c.setdefault("max_tokens", 8192)
    return c


def available() -> bool:
    return bool(cfg().get("api_key"))


def info() -> dict:
    c = cfg()
    return {"llm_configured": bool(c.get("api_key")), "llm_base_url": c.get("base_url"),
            "llm_model": c.get("model"), "vision": bool(c.get("vision"))}


def chat(messages: list[dict], temperature: float = 0.4, max_tokens: int | None = None) -> str:
    c = cfg()
    if not c.get("api_key"):
        raise RuntimeError(
            "LLM 未配置：设 CONFLOW_LLM_API_KEY（或写入 ~/.conflow/config.json，"
            "服务端在管理后台「系统配置」填写）")
    body = json.dumps({"model": c.get("model"), "messages": messages,
                       "temperature": temperature,
                       "max_tokens": max_tokens or int(c.get("max_tokens") or 8192)}).encode()
    req = urllib.request.Request(
        c["base_url"].rstrip("/") + "/chat/completions", data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {c['api_key']}"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        data = json.loads(resp.read())
    return data["choices"][0]["message"]["content"]


def test_connection() -> tuple[bool, str]:
    try:
        out = chat([{"role": "user", "content": "回复两个字：正常"}],
                   temperature=0, max_tokens=16)
        return True, out.strip()[:40]
    except Exception as exc:  # noqa: BLE001
        return False, str(exc)[:200]


def image_part(path: pathlib.Path) -> dict:
    b64 = base64.b64encode(path.read_bytes()).decode()
    return {"type": "image_url", "image_url": {"url": f"data:image/jpeg;base64,{b64}"}}


def strip_fence(text: str) -> str:
    t = text.strip()
    if t.startswith("```"):
        t = t.split("\n", 1)[1] if "\n" in t else t
        if t.rstrip().endswith("```"):
            t = t.rstrip()[:-3]
    return t.strip()
