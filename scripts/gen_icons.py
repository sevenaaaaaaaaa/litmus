#!/usr/bin/env python3
"""生成桌面端图标（desktop/icons/*）。

契约：颜色用 oklch 声明（与 tokens.css 同源），脚本里换算到 sRGB——
仓库里不出现 hex 字面量。图形语义：石蕊显色渐变（酸红 → 中性紫 → 碱蓝）上的一枝鹿蕊——
分叉如鹿角，也是知识的分叉。几何与 docs/brand/litmus-icon.svg 同源。

用法：python3 scripts/gen_icons.py
产物：icon.png(1024) / 32x32.png / 64x64.png / 128x128.png / 128x128@2x.png
      / icon-512.png / icon.icns / icon.ico，以及 server/web/icon48.png（favicon）
"""
from __future__ import annotations

import math
import struct
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "desktop" / "icons"
SS = 4  # 超采样倍数


# ── oklch → sRGB ────────────────────────────────────────────────────────────
def oklch(l: float, c: float, h: float) -> tuple[int, int, int]:
    """l: 0..1，c: chroma，h: 角度。返回 8bit sRGB。"""
    hr = math.radians(h)
    a, b = c * math.cos(hr), c * math.sin(hr)
    l_ = l + 0.3963377774 * a + 0.2158037573 * b
    m_ = l - 0.1055613458 * a - 0.0638541728 * b
    s_ = l - 0.0894841775 * a - 1.2914855480 * b
    lc, mc, sc = l_**3, m_**3, s_**3
    lin = (
        +4.0767416621 * lc - 3.3077115913 * mc + 0.2309699292 * sc,
        -1.2684380046 * lc + 2.6097574011 * mc - 0.3413193965 * sc,
        -0.0041960863 * lc - 0.7034186147 * mc + 1.7076147010 * sc,
    )

    def enc(u: float) -> int:
        u = max(0.0, min(1.0, u))
        u = 12.92 * u if u <= 0.0031308 else 1.055 * u ** (1 / 2.4) - 0.055
        return round(u * 255)

    return tuple(enc(v) for v in lin)  # type: ignore[return-value]


STOPS = [  # 同色系蓝色渐变：左上浅蓝 → 右下深蓝（与 litmus-icon.svg 一致）
    (0.00, oklch(0.56, 0.18, 252)),
    (0.52, oklch(0.47, 0.17, 258)),
    (1.00, oklch(0.42, 0.16, 264)),
]
INK = oklch(0.98, 0.006, 250)


def _lerp(c0, c1, t):
    return tuple(round(c0[i] + (c1[i] - c0[i]) * t) for i in range(3))


def gradient(size: int) -> Image.Image:
    """同色系蓝色渐变 + 顶部柔光（radial，白 18% → 0）。"""
    img = Image.new("RGB", (size, size))
    px = img.load()
    n = size - 1
    for y in range(size):
        for x in range(size):
            t = (x + y) / (2 * n)
            for (t0, c0), (t1, c1) in zip(STOPS, STOPS[1:]):
                if t <= t1:
                    col = _lerp(c0, c1, (t - t0) / (t1 - t0))
                    break
            dx, dy = x / n - 0.5, y / n - 0.18
            g = max(0.0, 1 - math.hypot(dx, dy) / 0.8) * 0.18
            px[x, y] = tuple(round(v + (255 - v) * g) for v in col)
    return img


def squircle_mask(size: int, n: float = 4.6) -> Image.Image:
    """超椭圆遮罩（|x|^n + |y|^n = 1），比圆角矩形更接近 macOS 图标轮廓。"""
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    r = size / 2
    pts = []
    for i in range(721):
        th = i * math.pi / 360
        ct, st = math.cos(th), math.sin(th)
        x = math.copysign(abs(ct) ** (2 / n), ct)
        y = math.copysign(abs(st) ** (2 / n), st)
        pts.append((r + x * r, r + y * r))
    d.polygon(pts, fill=255)
    return m


def _cubic(p0, p1, p2, p3, n=48):
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        yield (
            u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
            u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1],
        )


def draw_mark(img: Image.Image) -> None:
    """鹿蕊枝：主干 + 两次分枝 + 四个节点（1024 坐标系，按尺寸缩放）。"""
    s = img.size[0] / 1024
    d = ImageDraw.Draw(img, "RGBA")
    w = 58 * s
    strokes = [
        [(500, 812), (500, 532)],
        list(_cubic((500, 572), (500, 472), (418, 442), (378, 372))),
        list(_cubic((500, 532), (500, 442), (588, 412), (628, 332))),
        [(378, 372), (318, 272)], [(378, 372), (418, 262)],
        [(628, 332), (588, 232)], [(628, 332), (708, 262)],
    ]
    for pts in strokes:
        pts = [(x * s, y * s) for x, y in pts]
        # 沿路径密集盖圆章：圆头圆角、曲线无接缝
        for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
            k = max(1, int(math.hypot(x1 - x0, y1 - y0) / (w * 0.08)))
            for i in range(k + 1):
                x, y = x0 + (x1 - x0) * i / k, y0 + (y1 - y0) * i / k
                d.ellipse([x - w / 2, y - w / 2, x + w / 2, y + w / 2], fill=INK)
    for cx, cy in [(318, 272), (418, 262), (588, 232), (708, 262)]:
        r = 46 * s
        d.ellipse([cx * s - r, cy * s - r, cx * s + r, cy * s + r], fill=INK)


def master(size: int) -> Image.Image:
    big = size * SS
    base = gradient(big).convert("RGBA")
    draw_mark(base)
    base.putalpha(squircle_mask(big))
    return base.resize((size, size), Image.LANCZOS)


def mac_padded(m1024: Image.Image) -> Image.Image:
    """macOS 图标网格：主体 824/1024 居中 + 柔和投影，Dock 里与系统图标等大。"""
    from PIL import ImageFilter

    body = m1024.resize((824, 824), Image.LANCZOS)
    canvas = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    sh = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    sh.paste((0, 0, 0, 90), (100, 112), body.getchannel("A"))
    sh = sh.filter(ImageFilter.GaussianBlur(14))
    canvas.alpha_composite(sh)
    canvas.alpha_composite(body, (100, 100))
    return canvas


def write_icns(png_by_type: dict[bytes, bytes], path: Path) -> None:
    """最小 icns 写入器：magic + 逐块 (OSType, len, PNG)。"""
    body = b"".join(t + struct.pack(">I", len(p) + 8) + p for t, p in png_by_type.items())
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    m1024 = master(1024)

    sizes = {
        "icon.png": 1024,
        "icon-512.png": 512,
        "128x128@2x.png": 256,
        "128x128.png": 128,
        "64x64.png": 64,
        "32x32.png": 32,
    }
    rendered: dict[int, Image.Image] = {}
    for name, sz in sizes.items():
        im = m1024 if sz == 1024 else m1024.resize((sz, sz), Image.LANCZOS)
        rendered[sz] = im
        im.save(OUT / name)

    # icns：ic07..ic14（128/256/512/1024 各 1x/2x）
    mac = mac_padded(m1024)

    def enc(sz: int) -> bytes:
        import io

        buf = io.BytesIO()
        (mac if sz == 1024 else mac.resize((sz, sz), Image.LANCZOS)).save(buf, "PNG")
        return buf.getvalue()

    write_icns(
        {
            b"ic07": enc(128),
            b"ic08": enc(256),
            b"ic09": enc(512),
            b"ic10": enc(1024),
            b"ic11": enc(32),
            b"ic12": enc(64),
            b"ic13": enc(256),
            b"ic14": enc(512),
        },
        OUT / "icon.icns",
    )

    m1024.save(OUT / "icon.ico", sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    m1024.resize((48, 48), Image.LANCZOS).save(OUT.parent.parent / "server" / "web" / "icon48.png")
    print("icons ->", OUT)


if __name__ == "__main__":
    main()
