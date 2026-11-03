"""ConFlow 引擎 · 语义生成管线：分类 → 文档 → 幻灯片。

把 prompts/ 方法论组装为 LLM 提示词，产物与所有运行形态完全同构：
output/<video_id>/doc.md + slides.html。LLM 配置经 engine.llm provider 注入。
"""
from __future__ import annotations

import json
import pathlib
import re

from . import llm, plugins

ROOT = pathlib.Path(__file__).resolve().parent.parent
PROMPTS = ROOT / "prompts"
TEMPLATE = ROOT / "templates" / "slides.html"
PLUGINS = plugins.PLUGINS_DIR

DOC_TYPES = {
    "tutorial": "doc-tutorial.md",
    "science": "doc-science.md",
    "commentary": "doc-commentary.md",
    "other": "doc-other.md",
}
plugins.register_doc_types()   # 插件的 doc-<type>.md 注册为扩展文体

# 产出语言（zh=默认中文不加指令；其余注入硬性语言指令，防幻觉规则不随语言变）
LANGUAGES = {
    "zh": None, "en": "English", "ja": "日本語", "ko": "한국어",
    "es": "Español", "fr": "Français", "de": "Deutsch",
    "pt": "Português", "ru": "Русский", "ar": "العربية",
}
_HTML_LANG = {"zh": "zh-CN"}


def lang_directive(language: str) -> str:
    name = LANGUAGES.get((language or "zh").lower())
    if not name:
        return ""
    return (f"\n\n## 输出语言（硬性要求）\n"
            f"全部产出使用 **{name}** 撰写：标题、正文、要点、引用、图表标注、页脚注记一律用 {name}；\n"
            f"代码、命令、专有名词、API 名保留原文；时间戳 ⏱ 与标注体系（💡 补全、✅⚠️❌）格式保持不变，"
            f"说明文字也用 {name}。\n"
            f"防幻觉规则不因语言改变：数字与命令逐字核对、补全显式标注、观点归属分离。")


def html_lang(language: str) -> str:
    code = (language or "zh").lower()
    return _HTML_LANG.get(code, code)


def _read(p: pathlib.Path) -> str:
    return p.read_text(encoding="utf-8")


def _transcript_tail(transcript_md: str, limit: int = 60000) -> str:
    """超长转写只保留尾部（方法论重点在结构而非开头寒暄）。"""
    if len(transcript_md) <= limit:
        return transcript_md
    return "（前文过长已截断）\n" + transcript_md[-limit:]


# ------------------------------------------------------------- 1. 分类 ----

def classify(meta: dict, transcript_md: str) -> str:
    system = _read(PROMPTS / "classify.md")
    user = (f"视频元信息：{json.dumps({k: meta.get(k) for k in ('title','uploader','duration_sec')}, ensure_ascii=False)}\n\n"
            f"转写稿：\n{_transcript_tail(transcript_md)}\n\n"
            "只输出一个英文单词：tutorial / science / commentary / other。")
    out = llm.chat([{"role": "system", "content": system},
                    {"role": "user", "content": user}], temperature=0.1, max_tokens=8)
    out = out.strip().lower()
    for word in DOC_TYPES:
        if word in out:
            return word
    return "other"


# ------------------------------------------------------------- 2. 文档 ----

def gen_doc(dtype: str, meta: dict, transcript_md: str,
            sheet: pathlib.Path | None, language: str = "zh") -> str:
    system = _read(PROMPTS / DOC_TYPES[dtype]) + lang_directive(language)
    user = [f"视频元信息：{json.dumps(meta, ensure_ascii=False, default=str)[:2000]}\n\n"
            f"转写稿（[mm:ss] 为时间戳）：\n{_transcript_tail(transcript_md)}\n\n"
            "按方法论产出 doc.md 正文。只输出 Markdown 正文本身，不要围栏、不要解释。"]
    if sheet and sheet.exists() and llm.cfg().get("vision"):
        user = [{"type": "text", "text": user[0]},
                llm.image_part(sheet)]
        messages = [{"role": "system", "content": system},
                    {"role": "user", "content": user}]
    else:
        messages = [{"role": "system", "content": system},
                    {"role": "user", "content": user[0]}]
    return llm.strip_fence(llm.chat(messages, temperature=0.35))


# ---------------------------------------------------------- 3. 幻灯片 ----

def gen_slides(dtype: str, doc_md: str, frames: list[dict],
               available_frame_files: set[str], language: str = "zh") -> str:
    system = (_read(PROMPTS / "slides.md")
              + lang_directive(language)
              + "\n\n## 引擎组件速查（templates/slides.html 内置，直接用类名）\n"
              "页面骨架：<section class=\"slide\"> 可选属性 cover/section/closing；页内："
              ".kicker 小标签、h2 标题、.hrule 装饰线、.body 内容区（自动防溢出）、.note 页脚；"
              "要点 ul.bullets(.sm)、多栏 .cols>.col(.card)、引用 p.quote>.who、"
              "代码 .code>.bar+pre（.kw/.cm 着色）、数据卡 .stats>.stat>.num+.lbl、"
              "表格 table.t、图片 figure.figure>img+figcaption、流程 .flow>.step+.arr、"
              "对话气泡 .chat>.bubble(.r)>.who；入场动画给元素加 class frag 和 style=\"--i:序号\"。\n"
              f"主题：由系统注入（不要在片段里设置 data-theme）。")
    frames_note = "\n".join(
        f"- {f['file']}（⏱ {int(f['t']//60):02d}:{int(f['t']%60):02d}）"
        for f in frames if f["file"] in available_frame_files)
    user = (f"文档全文（幻灯片的唯一内容来源）：\n\n{doc_md}\n\n"
            f"可嵌入的原视频帧（相对路径 frames/<文件名>，只挑含独有信息的，最多 4 张）：\n{frames_note}\n\n"
            "产出幻灯片 <section> 片段：只输出一个个 <section class=\"slide\">…</section>，"
            "不要 <html>/<body>/引擎代码/围栏/解释；第一页带 class \"slide cover active\"。")
    return llm.strip_fence(llm.chat([{"role": "system", "content": system},
                                     {"role": "user", "content": user}],
                                    temperature=0.5))


# ------------------------------------------------- 4. 短视频口播脚本 ----

def gen_short_script(dtype: str, doc_md: str, meta: dict, language: str = "zh") -> str:
    """doc.md → 45–90s 口播脚本（节拍表与 v2video storyboard 同构）。"""
    system = (_read(PROMPTS / "script-short.md") + lang_directive(language)
              + "\n\n文体背景：源视频判定为 " + dtype + "，脚本口吻随之调整（教程偏步骤、评论偏观点、科普偏直觉）。")
    user = (f"doc.md 全文（唯一内容来源）：\n\n{doc_md}\n\n"
            f"视频元信息：{json.dumps({k: meta.get(k) for k in ('title', 'uploader', 'url')}, ensure_ascii=False)}\n\n"
            "按方法论产出 script.md。只输出脚本本身（Markdown），不要围栏、不要解释。")
    return llm.strip_fence(llm.chat([{"role": "system", "content": system},
                                     {"role": "user", "content": user}],
                                    temperature=0.5))


def build_deck(fragment: str, title: str, theme: str, language: str = "zh") -> str:
    """把生成的 <section> 片段注入幻灯片引擎，产出自包含 slides.html。"""
    tpl = _read(TEMPLATE)
    fragment = fragment.strip()
    if "<section" not in fragment:
        raise ValueError("模型未返回 <section> 片段")
    if fragment.startswith("`"):  # 保险：剥离围栏残迹
        fragment = fragment.strip("`")
    start = tpl.index("  <!-- ▼▼ 示例页")
    end = tpl.index("</div>\n\n<div id=\"progress\">")
    out = tpl[:start] + fragment + "\n  " + tpl[end:]
    out = re.sub(r'<html lang="zh-CN"( data-theme="[^"]*")?>',
                 f'<html lang="{html_lang(language)}" data-theme="{theme}">', out, count=1)
    out = out.replace("<title>ConFlow 幻灯片</title>", f"<title>{title}</title>")
    plugin_css = "\n".join(
        f'<style data-plugin="{p.parts[-2]}/{p.parts[-1]}">{css}</style>'
        for p, css in zip(sorted(PLUGINS.glob("*/themes.css")), plugins.themes_css()))
    if plugin_css:
        out = out.replace("</head>", plugin_css + "\n</head>", 1)
    return out
