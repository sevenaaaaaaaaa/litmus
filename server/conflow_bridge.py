#!/usr/bin/env python3
"""Litmus → ConFlow 的薄桥：只组装上游现有引擎，不复制生成方法论。"""
from __future__ import annotations

import argparse
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent
if not (ROOT / "engine").is_dir():
    ROOT = ROOT.parent / "third_party" / "conflow"
sys.path.insert(0, str(ROOT))

from engine import generate, llm, qc, storyboard  # noqa: E402


def html_animation(doc: str, language: str) -> str:
    system = (ROOT / "prompts" / "reverse-htmlanim.md").read_text(encoding="utf-8")
    system += generate.lang_directive(language)
    user = (
        "下面是唯一内容来源。请按方法论产出一个可直接打开、依赖内联的完整 HTML 动画；"
        "只输出 HTML，不要围栏或解释。\n\n" + doc
    )
    return llm.strip_fence(
        llm.chat(
            [{"role": "system", "content": system}, {"role": "user", "content": user}],
            temperature=0.5,
        )
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--options", required=True)
    ap.add_argument("--existing-doc", action="store_true")
    args = ap.parse_args()
    source = pathlib.Path(args.source).read_text(encoding="utf-8")
    out = pathlib.Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    opt = json.loads(pathlib.Path(args.options).read_text(encoding="utf-8"))
    language = opt.get("language") or "zh"
    dtype = opt.get("genre") or "auto"
    meta = {"title": opt.get("title") or pathlib.Path(args.source).stem,
            "uploader": "Litmus", "url": opt.get("source") or ""}
    if dtype == "auto" and not args.existing_doc:
        print("[conflow] LLM：判定文体…", file=sys.stderr, flush=True)
        dtype = generate.classify(meta, source)
    elif dtype == "auto":
        dtype = "other"
    print(f"[conflow] 文体：{dtype}", file=sys.stderr, flush=True)
    if args.existing_doc:
        doc = source
    else:
        print("[conflow] LLM：撰写文档…", file=sys.stderr, flush=True)
        doc = generate.gen_doc(dtype, meta, source, None, language=language)
    doc_path = out / "doc.md"
    doc_path.write_text(doc, encoding="utf-8")
    result: dict = {"title": meta["title"], "doc_type": dtype, "language": language,
                    "doc": str(doc_path)}
    if opt.get("slides"):
        print("[conflow] LLM：编排幻灯片…", file=sys.stderr, flush=True)
        fragment = generate.gen_slides(dtype, doc, [], set(), language=language)
        deck = out / "slides.html"
        deck.write_text(generate.build_deck(
            fragment, meta["title"], opt.get("theme") or "auto", language=language
        ), encoding="utf-8")
        result["slides"] = str(deck)
        result["qc"] = qc.check_deck(deck)
    if opt.get("script"):
        print("[conflow] LLM：撰写短视频脚本…", file=sys.stderr, flush=True)
        script = out / "script.md"
        script.write_text(generate.gen_short_script(
            dtype, doc, meta, language=language
        ), encoding="utf-8")
        result["script"] = str(script)
    if opt.get("storyboard"):
        print("[conflow] LLM：生成分镜物料包…", file=sys.stderr, flush=True)
        sb = storyboard.gen_storyboard(
            doc, meta, language=language, duration=int(opt.get("duration") or 60)
        )
        result["storyboard"] = str(storyboard.write_storyboard(out, sb))
    if opt.get("html_animation"):
        print("[conflow] LLM：生成 HTML 动画…", file=sys.stderr, flush=True)
        anim = out / "animation.html"
        anim.write_text(html_animation(doc, language), encoding="utf-8")
        result["animation"] = str(anim)
    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
