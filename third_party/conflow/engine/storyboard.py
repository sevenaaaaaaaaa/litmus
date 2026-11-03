"""ConFlow 引擎 · 视频直出阶段一：渲染接口 storyboard/。

doc.md → LLM 产出 storyboard.json（reverse-storyboard.md 的分镜数据结构，
即未来渲染器的输入接口）；narration.md / subs.srt / README 由 shots 确定性派生。
渲染成片（Remotion / 云端渲染）是阶段二，接口已按其输入冻结。
"""
from __future__ import annotations

import json
import pathlib
import re

from . import llm

PROMPT = pathlib.Path(__file__).resolve().parent.parent / "prompts" / "reverse-storyboard.md"


def _sec_to_srt(ts: float) -> str:
    ms = int(round(ts * 1000))
    h, ms = divmod(ms, 3600000)
    m, ms = divmod(ms, 60000)
    s, ms = divmod(ms, 1000)
    return f"{h:02d}:{m:02d}:{s:02d},{ms:03d}"


def gen_storyboard(doc_md: str, meta: dict, language: str = "zh",
                   duration: int = 60) -> dict:
    """LLM 产出 shots；返回 {"data": storyboard dict, "narration": str, "srt": str}。"""
    from .generate import lang_directive
    system = (_read(PROMPT) + lang_directive(language) +
              "\n\n## 本次自动化约定\n"
              f"目标总时长约 {duration} 秒（±20%）；只输出 storyboard.json 本体（一个 JSON 对象，"
              "不要围栏不要解释）；prompts 字段 sora/veo 用英文、kling 用中文；"
              "narration 口语化且与 sec 匹配（中文 ≈ 4 字/秒）；"
              "数字与事实只允许来自下方 doc.md。")
    user = (f"doc.md 全文（唯一内容来源）：\n\n{doc_md}\n\n"
            f"视频元信息：{json.dumps({k: meta.get(k) for k in ('title', 'uploader', 'url')}, ensure_ascii=False)}\n\n"
            "产出 storyboard.json。")
    raw = llm.strip_fence(llm.chat([{"role": "system", "content": system},
                                    {"role": "user", "content": user}],
                                   temperature=0.5))
    m = re.search(r"\{.*\}", raw, re.S)
    data = json.loads(m.group(0) if m else raw)
    if not data.get("shots"):
        raise ValueError("storyboard.json 缺少 shots")
    return {"data": data, **_derive(data, meta)}


def _read(p: pathlib.Path) -> str:
    return p.read_text(encoding="utf-8")


def _derive(data: dict, meta: dict) -> dict:
    shots = data["shots"]
    t = 0.0
    narration_lines = [f"# {data.get('title', '')} · 旁白脚本\n"]
    srt = []
    for i, shot in enumerate(shots, 1):
        sec = float(shot.get("sec") or 5)
        text = (shot.get("narration") or "").strip()
        narration_lines.append(f"## 镜头 {i}（{sec:g}s）\n\n{text}\n")
        srt.append(f"{i}\n{_sec_to_srt(t)} --> {_sec_to_srt(t + sec)}\n{text}\n")
        t += sec
    readme = (
        f"# {data.get('title', '')} · 分镜物料包\n\n"
        f"总时长 ≈ {t:.0f}s · 画幅 {data.get('aspect', '16:9')} · 风格：{data.get('style', '')}\n\n"
        "- `storyboard.json`：渲染接口（shots 含 narration/scene/motion/on_screen 与 sora/veo/kling 提示词）\n"
        "- `narration.md`：旁白脚本（先录音 / TTS）\n"
        "- `subs.srt`：与旁白对齐的字幕\n"
        f"- 来源视频：{meta.get('url')}\n\n"
        "工作流：旁白先行 → 逐镜头用 prompts 生成画面 → 剪辑对齐；"
        "数字与专名镜头用 on_screen 后期上字。")
    return {"narration": "\n".join(narration_lines), "srt": "\n".join(srt), "readme": readme}


def write_storyboard(workdir: pathlib.Path, result: dict) -> pathlib.Path:
    out = workdir / "storyboard"
    out.mkdir(exist_ok=True)
    (out / "storyboard.json").write_text(
        json.dumps(result["data"], ensure_ascii=False, indent=2), encoding="utf-8")
    (out / "narration.md").write_text(result["narration"], encoding="utf-8")
    (out / "subs.srt").write_text(result["srt"], encoding="utf-8")
    (out / "README.md").write_text(result["readme"], encoding="utf-8")
    return out
