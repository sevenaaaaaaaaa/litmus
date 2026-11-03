#!/usr/bin/env python3
"""ConFlow CLI — 一条命令把视频变成可上线的文章与幻灯片（本机直转，无需服务端/ZCode）。

用法:
  python3 bin/conflow.py convert <url> [--doc-type auto] [--theme auto]
                                       [--max-frames 24] [--max-items 5]
                                       [--no-slides] [--no-doc] [--json]
  python3 bin/conflow.py config                 # 查看当前 LLM 配置（脱敏）

<url> 支持单视频，也支持频道 / 播放列表（自动展开前 --max-items 条逐个转换）。

配置（优先级：环境变量 > ~/.conflow/config.json）:
  export CONFLOW_LLM_API_KEY=sk-…               # 任何 OpenAI 兼容接口
  export CONFLOW_LLM_BASE_URL=https://open.bigmodel.cn/api/paas/v4
  export CONFLOW_LLM_MODEL=glm-4.7
  或写入 ~/.conflow/config.json: {"llm": {"api_key": "sk-…", "base_url": "…", "model": "…"}}

依赖: yt-dlp（素材；无字幕视频推荐 faster-whisper）。语义引擎在本仓库 engine/，
无需 FastAPI / 数据库 / 浏览器（质检自动回退启发式）。
产出: output/<video_id>/doc.md + slides.html（30 主题、带质检报告）。
"""
from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import pathlib
import re
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "bin"))

from engine import batch, generate, llm, qc, storyboard  # noqa: E402
import v2h  # noqa: E402

OUT = ROOT / "output"
THEME_MAP = {"tutorial": "tutorial", "science": "science", "commentary": "commentary"}
LANG_CHOICES = list(generate.LANGUAGES)


def log(msg: str) -> None:
    print(f"[conflow] {msg}", file=sys.stderr, flush=True)


def _find_new(before: set[str], fetch_out: str) -> str:
    """fetch 的产物目录：优先比对目录差集，失败再从 JSON 摘要抠 id。"""
    new = {p.name for p in OUT.glob("*") if p.is_dir()} - before
    if len(new) == 1:
        return new.pop()
    m = re.search(r'"id":\s*"([\w-]+)"', fetch_out)
    if m:
        return m.group(1)
    raise RuntimeError("无法定位产物目录（fetch 输出异常）")


def _convert_one(url: str, args) -> dict:
    """单个视频的全链路。返回 summary dict；失败抛异常。"""
    t0 = time.time()
    before = {p.name for p in OUT.glob("*") if p.is_dir()}
    log(f"抓取素材：{url}")
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = v2h.fetch(url, args.max_frames, do_frames=True)
    if rc != 0:
        raise RuntimeError("素材抓取失败（yt-dlp），详见上方日志")
    vid = _find_new(before, buf.getvalue())
    workdir = OUT / vid
    meta = v2h.describe(workdir)
    title = meta.get("title") or vid
    log(f"素材就绪 → {vid}（{meta.get('duration_sec', '?')}s）{title[:40]}")

    if not (workdir / "transcript.md").exists():
        log("无字幕，本地 Whisper 转写兜底…")
        if v2h.transcribe(vid) != 0:
            raise RuntimeError("转写失败（无字幕且无可用 Whisper：pip3 install faster-whisper）")
    transcript_md = (workdir / "transcript.md").read_text(encoding="utf-8")

    dtype = args.doc_type
    if dtype == "auto":
        log("LLM：判定文体…")
        dtype = generate.classify(meta, transcript_md)
    log(f"文体：{dtype}")

    doc_path = workdir / "doc.md"
    if args.no_doc and doc_path.exists():
        log("跳过文档（--no-doc 且已有 doc.md）")
    else:
        log(f"LLM：撰写文档…（语言：{args.language}）")
        sheet = workdir / "sheet.jpg"
        doc_md = generate.gen_doc(dtype, meta, transcript_md,
                                  sheet if llm.cfg().get("vision") else None,
                                  language=args.language)
        doc_path.write_text(doc_md, encoding="utf-8")
        log(f"文档完成 → {doc_path.relative_to(ROOT)}（{len(doc_md)} 字符）")

    qc_report = None
    slides_rel = None
    script_rel = None
    if not args.no_slides:
        log(f"LLM：编排幻灯片…（语言：{args.language}）")
        frames = json.loads((workdir / "frames.json").read_text(encoding="utf-8"))
        have = {f["file"] for f in frames}
        fragment = generate.gen_slides(dtype, doc_path.read_text(encoding="utf-8"), frames, have,
                                       language=args.language)
        theme = args.theme if args.theme != "auto" else THEME_MAP.get(dtype, "geist")
        deck = workdir / "slides.html"
        deck.write_text(generate.build_deck(fragment, title, theme, language=args.language),
                        encoding="utf-8")
        slides_rel = str(deck.relative_to(ROOT))
        log(f"幻灯片完成 → {slides_rel}（主题 {theme}）")

        log("质检每页布局…")
        qc_report = qc.check_deck(deck)
        q = qc_report
        log(f"质检[{q['mode']}]：{q['ok']} 通过 / {q['warn']} 警告 / {q['fail']} 不合格")
        for p in q["pages"]:
            if p["status"] != "ok":
                log(f"  ⚠ P{p['page']} {p['status']}：{p['reason']}")

    if args.script:
        log(f"LLM：短视频口播脚本…（语言：{args.language}）")
        script_path = workdir / "script.md"
        script_path.write_text(
            generate.gen_short_script(dtype, doc_path.read_text(encoding="utf-8"),
                                      meta, language=args.language), encoding="utf-8")
        script_rel = str(script_path.relative_to(ROOT))
        log(f"脚本完成 → {script_rel}")

    sb_rel = None
    if args.storyboard:
        log(f"LLM：分镜提示词包 storyboard/…（语言：{args.language}）")
        sb = storyboard.gen_storyboard(doc_path.read_text(encoding="utf-8"), meta,
                                       language=args.language, duration=args.sb_duration)
        sb_dir = storyboard.write_storyboard(workdir, sb)
        sb_rel = str(sb_dir.relative_to(ROOT))
        log(f"分镜物料包完成 → {sb_rel}/（storyboard.json · narration.md · subs.srt · README）")

    return {"video_id": vid, "title": title, "doc_type": dtype, "language": args.language,
            "doc": str(doc_path.relative_to(ROOT)), "slides": slides_rel, "script": script_rel,
            "storyboard": sb_rel,
            "qc": qc_report, "elapsed_sec": round(time.time() - t0, 1)}


def cmd_convert(args) -> int:
    if not llm.available():
        log("LLM 未配置。二选一：")
        log("  export CONFLOW_LLM_API_KEY=sk-…   # 任何 OpenAI 兼容接口")
        log('  或写入 ~/.conflow/config.json: {"llm": {"api_key": "…", "base_url": "…", "model": "…"}}')
        return 2
    c = llm.cfg()
    log(f"LLM：{c['model']} @ {c['base_url'].split('//')[-1].rstrip('/')}")

    if batch.looks_batch(args.url):
        entries = batch.expand(args.url, limit=args.max_items, proxy=v2h.detect_proxy())
        log(f"列表展开：{len(entries)} 个视频，逐个转换\n")
        results, failed = [], 0
        for i, e in enumerate(entries, 1):
            log(f"—— [{i}/{len(entries)}] {e['title'][:50]} ——")
            try:
                results.append(_convert_one(e["url"], args))
            except Exception as exc:  # noqa: BLE001  单个失败不拖垮整批
                failed += 1
                log(f"✗ 失败：{exc}")
        log(f"批量完成：成功 {len(results)} / 失败 {failed}")
        if args.json:
            print(json.dumps({"results": results, "failed": failed}, ensure_ascii=False))
        return 0 if failed == 0 else 1

    try:
        summary = _convert_one(args.url, args)
    except Exception as exc:  # noqa: BLE001
        log(f"失败：{exc}")
        return 1
    if args.json:
        print(json.dumps(summary, ensure_ascii=False))
    else:
        log(f"完成，用时 {summary['elapsed_sec']}s")
        print(f"\n  📄 {summary['doc']}")
        if summary["slides"]:
            print(f"  🖥 {summary['slides']}   （浏览器打开放映：→ 翻页 · T 换主题 · P 导出 PDF）")
        if summary["script"]:
            print(f"  🎬 {summary['script']}")
        if summary.get("storyboard"):
            print(f"  🎞 {summary['storyboard']}/")
        print()
    return 0


def cmd_config(_args) -> int:
    c = llm.cfg()
    masked = {**c, "api_key": (c.get("api_key") or "")[:6] + "***" if c.get("api_key") else "(未设置)"}
    print(json.dumps(masked, ensure_ascii=False, indent=2))
    src = "环境变量" if os.environ.get("CONFLOW_LLM_API_KEY") else \
          ("~/.conflow/config.json" if (pathlib.Path.home() / ".conflow/config.json").exists()
           else "（默认值，未配置 key）")
    print(f"# 来源：{src}", file=sys.stderr)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(prog="conflow", description="ConFlow 本机直转：视频 → 文章 + 幻灯片")
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("convert", help="一条命令完成抓取 → 文档 → 幻灯片 → 质检")
    p.add_argument("url", help="视频 / 频道 / 播放列表 URL")
    p.add_argument("--doc-type", choices=["auto", "tutorial", "science", "commentary", "other"],
                   default="auto")
    p.add_argument("--language", choices=LANG_CHOICES, default="zh",
                   help="产出语言（默认中文；en/ja/ko/es/fr/de/pt/ru/ar）")
    p.add_argument("--theme", default="auto", help="auto 跟随文体，或 30 主题名")
    p.add_argument("--max-frames", type=int, default=24)
    p.add_argument("--max-items", type=int, default=5, help="频道/播放列表最多转换几个")
    p.add_argument("--no-slides", action="store_true", help="只产出文档")
    p.add_argument("--script", action="store_true", help="追加产出 45–90s 短视频口播脚本 script.md")
    p.add_argument("--storyboard", action="store_true", help="追加产出分镜物料包 storyboard/（视频直出渲染接口）")
    p.add_argument("--sb-duration", type=int, default=60, help="分镜目标总时长（秒）")
    p.add_argument("--no-doc", action="store_true", help="跳过文档重写（配合已有 doc.md 只做幻灯片）")
    p.add_argument("--json", action="store_true", help="结果以 JSON 输出到 stdout（供脚本/插件调用）")
    p.set_defaults(fn=cmd_convert)

    p = sub.add_parser("config", help="查看当前 LLM 配置（脱敏）")
    p.set_defaults(fn=cmd_config)

    args = ap.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
