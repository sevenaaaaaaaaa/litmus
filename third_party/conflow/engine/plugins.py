"""ConFlow 引擎 · 插件规范 v1（文件即约定，对齐 OpenFlow PluginSystem 心智）。

plugins/<插件ID>/ 目录：
  plugin.json   必须。{"name","version","description","author", 可选 "themes":["主题名",…]}
  push.py       可选。实现 push(workdir, meta, doc_md, cfg) -> dict，
                注册为推送目标（目标名 = 插件目录名）
  doc-<type>.md 可选。注册新文体写作法（type 即 doc_type 取值）
  themes.css    可选。注入幻灯片引擎的额外主题样式
                 （[data-theme="…"]{--bg:…} 六变量起步；themes 字段声明主题名）

服务端启动与 CLI 运行时各加载一次；插件失败不阻塞主流程（跳过并打印告警）。
"""
from __future__ import annotations

import importlib.util
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLUGINS_DIR = ROOT / "plugins"

_cache: dict | None = None


def discover(force: bool = False) -> dict:
    """扫描 plugins/，返回 {"push": {name: fn}, "doc_types": {type: path},
    "themes_css": [css 文本], "themes": [主题名], "manifests": [...]}。"""
    global _cache
    if _cache is not None and not force:
        return _cache
    reg = {"push": {}, "doc_types": {}, "themes_css": [], "themes": [], "manifests": []}
    if not PLUGINS_DIR.is_dir():
        _cache = reg
        return reg
    for pdir in sorted(PLUGINS_DIR.iterdir()):
        try:
            part = _load_one(pdir)
        except Exception as e:  # noqa: BLE001  单插件失败不拖垮主流程
            print(f"[plugins] 插件 {pdir.name} 加载失败，已跳过：{e}", file=sys.stderr)
            continue
        reg["push"].update(part.get("push", {}))
        reg["doc_types"].update(part.get("doc_types", {}))
        reg["themes_css"].extend(part.get("themes_css", []))
        reg["themes"].extend(part.get("themes", []))
        reg["manifests"].extend(part.get("manifests", []))
    _cache = reg
    return reg


def _load_one(pdir: pathlib.Path) -> dict:
    if not pdir.is_dir():
        return {}
    manifest_p = pdir / "plugin.json"
    if not manifest_p.exists():
        return {}
    manifest = json.loads(manifest_p.read_text(encoding="utf-8"))
    out = {"push": {}, "doc_types": {}, "themes_css": [], "themes": [], "manifests": [manifest]}
    pid = pdir.name

    push_p = pdir / "push.py"
    if push_p.exists():
        spec = importlib.util.spec_from_file_location(f"conflow_plugin_{pid}_push", push_p)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        if hasattr(mod, "push"):
            out["push"][pid] = mod.push

    for p in sorted(pdir.glob("doc-*.md")):
        out["doc_types"][p.stem[4:]] = p

    css = pdir / "themes.css"
    if css.exists():
        out["themes_css"].append(css.read_text(encoding="utf-8"))
    out["themes"].extend(manifest.get("themes") or [])
    return out


def push_adapters() -> dict:
    """插件提供的推送适配器 {目标名: push 函数}。"""
    return discover()["push"]


def register_doc_types() -> None:
    """把插件的 doc-<type>.md 注册进 generate.DOC_TYPES（幂等）。"""
    from . import generate
    for dtype, path in discover()["doc_types"].items():
        generate.DOC_TYPES.setdefault(dtype, path)


def themes_css() -> list[str]:
    return discover()["themes_css"]


def plugin_themes() -> list[str]:
    return discover()["themes"]
