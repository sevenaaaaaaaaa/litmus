#!/usr/bin/env python3
"""ConFlow CLI — YouTube 视频素材管线（确定性部分）。

用法:
  python3 bin/v2h.py fetch <url> [--max-frames 24] [--no-frames]
  python3 bin/v2h.py frames <video_id> [--max-frames 24]
  python3 bin/v2h.py transcribe <video_id>      # 字幕缺失时兜底（需本地 Whisper）
  python3 bin/v2h.py status

产出目录约定: output/<video_id>/
  <id>.info.json  <id>.mp4  <id>*.srt  frames/fXXX_tSSSSs.jpg  sheet.jpg
  transcript.md / transcript.json  frames.json  (doc.md / slides.html 由 Agent 生成)
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "output"

HTTP_PROXY_CANDIDATES = [
    "http://127.0.0.1:6152",   # Surge HTTP
    "http://127.0.0.1:7890",   # Clash
    "http://127.0.0.1:7897",   # Clash Verge
    "http://127.0.0.1:1087",   # V2Ray legacy
    "http://127.0.0.1:8888",
]
SOCKS_PROXY_CANDIDATES = ["socks5://127.0.0.1:6153", "socks5://127.0.0.1:1080"]

# 字幕挑选：人工字幕优先；组内按偏好排序（zh 人工翻译常优于 en ASR 的机器翻译，反之 en-orig ASR 优于转译）
SUB_PREF_MANUAL = ["zh-CN", "zh-Hans", "zh-TW", "zh", "en"]
SUB_PREF_AUTO = ["en-orig", "en", "zh-Hans"]


def log(msg: str) -> None:
    print(msg, flush=True)


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


# ---------------------------------------------------------------- proxy ----

def detect_proxy() -> str | None:
    """依次尝试环境变量、HTTP 代理端口（真实验证可通 YouTube）、SOCKS 端口（TCP 探测）。"""
    for var in ("CONFLOW_PROXY", "V2HTML_PROXY", "https_proxy", "HTTPS_PROXY",
                "http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"):
        v = os.environ.get(var)
        if v:
            return v.rstrip("/")
    for p in HTTP_PROXY_CANDIDATES:
        try:
            opener = urllib.request.build_opener(
                urllib.request.ProxyHandler({"http": p, "https": p}))
            with opener.open("https://www.youtube.com/generate_204", timeout=4) as resp:
                if resp.status < 500:
                    return p
        except Exception:
            continue
    for p in SOCKS_PROXY_CANDIDATES:
        host, port = p.split("//")[1].split(":")
        try:
            with socket.create_connection((host, int(port)), timeout=1.0):
                return p
        except Exception:
            continue
    return None


# ---------------------------------------------------------------- fetch ----

YTDLP_FORMAT = (
    "bv*[height<=480][vcodec^=avc1]+ba[acodec^=mp4a]/"
    "b[height<=480]/bv*[height<=480]+ba/b"
)


def fetch(url: str, max_frames: int, do_frames: bool) -> int:
    proxy = detect_proxy()
    log(f"[proxy] {'已检测到 ' + proxy if proxy else '未发现代理，尝试直连'}")

    before = {p for p in OUT.glob("*") if p.is_dir()} if OUT.exists() else set()
    cmd = [sys.executable, "-m", "yt_dlp"]
    if proxy:
        cmd += ["--proxy", proxy]
    cmd += [
        "-f", YTDLP_FORMAT,
        "--merge-output-format", "mp4",
        "--write-info-json", "--write-thumbnail",
        "--write-subs", "--write-auto-subs",
        "--sub-langs", "en.*,zh.*",
        "--convert-subs", "srt",
        "--ignore-errors", "--retries", "10", "--retry-sleep", "3",
        "--extractor-retries", "3",
        "--no-playlist", "--no-progress", "--no-warnings",
        "-o", str(OUT / "%(id)s" / "%(id)s.%(ext)s"),
        url,
    ]
    r = run(cmd)
    if r.returncode != 0:
        sys.stderr.write(r.stderr[-3000:])
        log("[fetch] yt-dlp 失败")
        return 1

    after = {p for p in OUT.glob("*") if p.is_dir()}
    new_dirs = sorted(after - before, key=lambda p: p.stat().st_mtime)
    workdir = (new_dirs or sorted(after, key=lambda p: p.stat().st_mtime))[-1]
    summary = describe(workdir)
    log(json.dumps(summary, ensure_ascii=False, indent=2))

    if do_frames:
        n = build_frames(summary["id"], max_frames)
        log(f"[frames] 已生成 {n} 帧 → {workdir / 'frames'}，联览图 {workdir / 'sheet.jpg'}")
        if not build_transcript_from_subs(summary["id"]):
            log("[transcript] 没有可用字幕，可运行: "
                f"python3 bin/v2h.py transcribe {summary['id']}")
    return 0


def load_info(workdir: pathlib.Path) -> dict:
    infos = sorted(workdir.glob("*.info.json"))
    if infos:
        try:
            return json.loads(infos[0].read_text(encoding="utf-8"))
        except Exception:
            pass
    return {}


def describe(workdir: pathlib.Path) -> dict:
    info = load_info(workdir)
    return {
        "id": workdir.name,
        "dir": str(workdir),
        "title": info.get("title", ""),
        "uploader": info.get("uploader", ""),
        "url": info.get("webpage_url", ""),
        "duration_sec": int(info.get("duration") or 0),
        "chapters": info.get("chapters") or [],
        "subtitle_files": sorted(
            f.name for f in list(workdir.glob("*.srt")) + list(workdir.glob("*.vtt"))),
        "video": bool(list(workdir.glob("*.mp4"))),
        "frames": len(list((workdir / "frames").glob("*.jpg")))
        if (workdir / "frames").exists() else 0,
        "has_transcript": (workdir / "transcript.md").exists(),
    }


def pick_subtitle(workdir: pathlib.Path) -> pathlib.Path | None:
    """人工字幕优先，组内按语言偏好；实在没有再回退到任意字幕文件。"""
    info = load_info(workdir)
    manual = set(info.get("subtitles", {}).keys())
    auto = set(info.get("automatic_captions", {}).keys())

    def file_for(key: str) -> pathlib.Path | None:
        for ext in (".srt", ".vtt"):
            p = workdir / f"{workdir.name}.{key}{ext}"
            if p.exists():
                return p
        return None

    for pref, keys in ((SUB_PREF_MANUAL, manual), (SUB_PREF_AUTO, auto)):
        ordered = [k for k in pref if k in keys] + sorted(keys - set(pref))
        for k in ordered:
            f = file_for(k)
            if f:
                return f
    fallback = sorted(list(workdir.glob("*.srt")) + list(workdir.glob("*.vtt")))
    return fallback[0] if fallback else None


# --------------------------------------------------------------- frames ----

def build_frames(video_id: str, max_frames: int) -> int:
    workdir = OUT / video_id
    video = next(iter(sorted(workdir.glob("*.mp4"))), None)
    if not video:
        log(f"[frames] {workdir} 下没有视频文件")
        return 0
    frames_dir = workdir / "frames"
    tmp = workdir / ".frames_tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    shutil.rmtree(frames_dir, ignore_errors=True)
    tmp.mkdir(parents=True)

    dur = probe_duration(video)
    if dur <= 0:
        log("[frames] 无法读取视频时长")
        return 0

    # 1) 场景切变检测：切变点多则等距精选；少（静态口播）则均匀采样
    r = run(["ffmpeg", "-hide_banner", "-i", str(video),
             "-vf", "select='gt(scene,0.2)',showinfo",
             "-fps_mode", "vfr", "-frames:v", "400", "-q:v", "3",
             str(tmp / "f%04d.jpg")])
    times = sorted(float(m) for m in re.findall(r"pts_time:([0-9.]+)", r.stderr))
    if len(times) >= 6:
        step = max(1, len(times) // max_frames)
        shots = times[::step][:max_frames]
    else:
        shots = [dur * (k + 0.5) / max_frames for k in range(max_frames)]
    if not shots or shots[0] > 1.0:
        shots = [0.6] + shots

    # 2) 按时间序统一命名 f<序号:03d>_t<秒:04d>s.jpg（保证联览图顺序=时间序）
    frames_dir.mkdir(parents=True)
    manifest = []
    for i, t in enumerate(sorted(shots)):
        dst = frames_dir / f"f{i:03d}_t{int(round(t)):04d}s.jpg"
        r2 = run(["ffmpeg", "-hide_banner", "-y", "-ss", f"{t:.2f}",
                  "-i", str(video), "-frames:v", "1", "-q:v", "3", str(dst)])
        if r2.returncode == 0 and dst.exists():
            manifest.append({"t": round(t, 2), "file": dst.name})
    shutil.rmtree(tmp, ignore_errors=True)
    (workdir / "frames.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=1), encoding="utf-8")

    # 3) 联览图：Agent 一次浏览全部帧
    n = len(manifest)
    if n:
        cols = min(6, n)
        rows = (n + cols - 1) // cols
        run(["ffmpeg", "-hide_banner", "-y",
             "-pattern_type", "glob", "-i", str(frames_dir / "f*.jpg"),
             "-filter_complex",
             f"scale=480:-2,tile={cols}x{rows}:padding=4:color=0x101014",
             "-frames:v", "1", str(workdir / "sheet.jpg")])
    return n


def probe_duration(video: pathlib.Path) -> float:
    r = run(["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "json", str(video)])
    try:
        return float(json.loads(r.stdout)["format"]["duration"])
    except Exception:
        return 0.0


# ----------------------------------------------------------- transcript ----

TIMECODE = re.compile(r"(\d+):(\d+):(\d+)[,.](\d+)\s*-->")


def parse_srt(path: pathlib.Path) -> list[tuple[float, float, str]]:
    segs: list[tuple[float, float, str]] = []
    start = end = 0.0
    buf: list[str] = []

    def emit():
        if not buf:
            return
        text = " ".join(buf).strip()
        prev = segs[-1][2] if segs else None
        if text and text != prev:  # 自动字幕滚动会产生重复行
            segs.append((start, end, text))

    for raw in path.read_text(encoding="utf-8", errors="ignore").splitlines():
        m = TIMECODE.search(raw)
        if m:
            emit()
            buf = []
            h, mi, s, ms = (int(x) for x in m.groups())
            start = h * 3600 + mi * 60 + s + ms / 1000
            end = start
        elif raw.strip() and not raw.strip().isdigit() \
                and not raw.startswith(("WEBVTT", "NOTE")):
            buf.append(re.sub(r"<[^>]+>", "", raw).strip())
    emit()
    return segs


def dedupe_rolling(segs, max_overlap=15, min_overlap=3):
    """ASR 滚动字幕去重：行内重复块折叠 + 跨行词级重叠合并。人工字幕不动。"""
    out = []
    tail: list[str] = []
    for st, en, text in segs:
        words = text.split()
        # 1) 行内折叠：连续重复的 ≥3 词块只保留一次（A A A → A）
        i = 0
        kept: list[str] = []
        while i < len(words):
            hit = False
            for L in range(12, 2, -1):
                if i + 2 * L <= len(words) and words[i:i + L] == words[i + L:i + 2 * L]:
                    kept.extend(words[i:i + L])
                    i += 2 * L
                    hit = True
                    break
            if not hit:
                kept.append(words[i])
                i += 1
        words = kept
        # 2) 跨行重叠：新行开头与已输出内容的词尾重叠 ≥min_overlap 时只接续增量
        k = min(max_overlap, len(words), len(tail))
        overlap = 0
        for L in range(k, min_overlap - 1, -1):
            if L and tail[-L:] == words[:L]:
                overlap = L
                break
        new = words[overlap:]
        if new:
            out.append((st, en, " ".join(new)))
            tail = (tail + new)[-max_overlap:]
    return out


def build_transcript_from_subs(video_id: str) -> bool:
    workdir = OUT / video_id
    sub = pick_subtitle(workdir)
    if not sub:
        return False
    segs = parse_srt(sub)
    if not segs:
        return False
    info = load_info(workdir)
    m = re.search(rf"{re.escape(workdir.name)}\.([A-Za-z-]+)\.(srt|vtt)$", sub.name)
    key = m.group(1) if m else ""
    is_asr = key in info.get("automatic_captions", {}) \
        and key not in info.get("subtitles", {})
    if is_asr:
        segs = dedupe_rolling(segs)
    write_transcript(workdir, segs, sub.name)
    return True


def write_transcript(workdir: pathlib.Path, segs, src: str) -> None:
    info = load_info(workdir)
    lines = [
        f"# {info.get('title', workdir.name)}",
        "",
        f"- 来源: {info.get('webpage_url', '')}",
        f"- 作者: {info.get('uploader', '')}",
        f"- 时长: {int(info.get('duration') or 0)} 秒",
        f"- 字幕来源: {src}",
        "",
        "---",
        "",
    ]
    para: list[str] = []
    para_t = 0.0
    prev_end = 0.0

    def flush():
        nonlocal para
        if para:
            mm, ss = divmod(int(para_t), 60)
            lines.append(f"[{mm:02d}:{ss:02d}] " + " ".join(para))
            para = []

    for st, en, text in segs:
        if not para:
            para_t = st
        para.append(text)
        joined = " ".join(para)
        if (len(joined) >= 320
                or (len(joined) >= 80 and text[-1] in ".!?。！？")
                or st - prev_end > 3.0):
            flush()
        prev_end = en
    flush()
    (workdir / "transcript.md").write_text("\n".join(lines), encoding="utf-8")
    (workdir / "transcript.json").write_text(
        json.dumps([{"start": s, "end": e, "text": t} for s, e, t in segs],
                   ensure_ascii=False), encoding="utf-8")
    log(f"[transcript] {len(segs)} 条字幕 → transcript.md")


def transcribe(video_id: str) -> int:
    workdir = OUT / video_id
    if build_transcript_from_subs(video_id):
        log("[transcribe] 已有字幕，直接生成 transcript.md")
        return 0
    video = next(iter(sorted(workdir.glob("*.mp4"))), None)
    if not video:
        log("[transcribe] 找不到视频文件")
        return 1
    backend = None
    # CONFLOW_WHISPER_BACKEND 可强制指定（mlx_whisper / faster_whisper / whisper），便于测试与部署
    force = os.environ.get("CONFLOW_WHISPER_BACKEND")
    for mod in (([force] if force else []) + ["mlx_whisper", "faster_whisper", "whisper"]):
        if run([sys.executable, "-c", f"import {mod}"]).returncode == 0:
            backend = mod
            break
    if not backend:
        log("[transcribe] 无本地 Whisper 可用。安装其一：\n"
            "  pip3 install --user --break-system-packages mlx-whisper      # Apple Silicon 推荐\n"
            "  pip3 install --user --break-system-packages faster-whisper   # 无 torch，CPU 服务器推荐\n"
            "  pip3 install --user --break-system-packages openai-whisper")
        return 2
    log(f"[transcribe] 使用 {backend} 转写（首次运行需下载模型，耗时较长）…")
    seg_code = {
        "mlx_whisper":
            '[ (s["start"], s["end"], s["text"].strip()) '
            'for s in W.transcribe(r"%s", path_or_hf_repo="mlx-community/'
            'whisper-large-v3-turbo")["segments"] ]' % video,
        "whisper":
            '[ (s["start"], s["end"], s["text"].strip()) '
            'for s in W.transcribe(r"%s")["segments"] ]' % video,
        "faster_whisper":
            # 模型大小可调（CONFLOW_WHISPER_MODEL，默认 small 兼顾中文质量与 CPU 耗时）；
            # int8 量化 + VAD 过静音，服务器 CPU 可跑，PyAV 自带解码不依赖系统 ffmpeg
            '[ (s.start, s.end, s.text.strip()) for s in '
            'WhisperModel("%s", device="cpu", compute_type="int8")'
            '.transcribe(r"%s", vad_filter=True)[0] ]'
            % (os.environ.get("CONFLOW_WHISPER_MODEL", "small"), video),
    }[backend]
    code = f"""
import {backend} as W
{ 'from faster_whisper import WhisperModel' if backend == 'faster_whisper' else '' }
import json
segs = {seg_code}
json.dump([list(s) for s in segs], open(r"{workdir / '.whisper.json'}", "w"))
print(len(segs))
"""
    r = run([sys.executable, "-c", code])
    if r.returncode != 0:
        sys.stderr.write(r.stderr[-2000:])
        return 1
    segs = [tuple(s) for s in
            json.loads((workdir / ".whisper.json").read_text())]
    write_transcript(workdir, segs, f"whisper:{backend}")
    return 0


# ---------------------------------------------------------------- status ----

def status() -> int:
    if not OUT.exists():
        log("(output/ 为空)")
        return 0
    for d in sorted(OUT.iterdir()):
        if not d.is_dir():
            continue
        s = describe(d)
        flags = [f for f in ("transcript", "doc", "slides")
                 if d.joinpath({"transcript": "transcript.md",
                                "doc": "doc.md",
                                "slides": "slides.html"}[f]).exists()]
        print(f"{s['id'][:12]:<12} {s['duration_sec'] // 60:>3}m "
              f"frames={s['frames']:<3} {'|'.join(flags) or '-':<28} {s['title'][:48]}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(prog="v2h", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("fetch")
    p.add_argument("url")
    p.add_argument("--max-frames", type=int, default=24)
    p.add_argument("--no-frames", action="store_true")
    p = sub.add_parser("frames")
    p.add_argument("video_id")
    p.add_argument("--max-frames", type=int, default=24)
    p = sub.add_parser("transcribe")
    p.add_argument("video_id")
    sub.add_parser("status")
    a = ap.parse_args()
    OUT.mkdir(exist_ok=True)

    if a.cmd == "fetch":
        return fetch(a.url, a.max_frames, not a.no_frames)
    if a.cmd == "frames":
        n = build_frames(a.video_id, a.max_frames)
        log(f"[frames] {n} 帧 → output/{a.video_id}/frames/, 联览图 sheet.jpg")
        return 0 if n else 1
    if a.cmd == "transcribe":
        return transcribe(a.video_id)
    return status()


if __name__ == "__main__":
    sys.exit(main())
