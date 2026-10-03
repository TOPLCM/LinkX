#!/usr/bin/env python3
"""把 `Temp/svg-assets.json` 里的图标按 Windows 侧边栏的真实排布渲染成 PNG，供肉眼验收。

为什么单独一个脚本：图标这件事只在像素上可判定。Rust 侧的
`icons::tests::render_icons_into_bitmap` 已经能出 GDI 实际渲染结果，但它跑在
测试里、只出 PPM、且不会随图标包改动自动更新；这个脚本用于**改生成器时**
立刻看到"两端同源的那份几何"长什么样（安卓 VectorDrawable 用的就是同一份）。

用法：
    python Scripts/nav_preview.py                 # 输出 Temp/svg-preview/nav-at-28.png
    python Scripts/nav_preview.py --size 28 --ss 4
"""

import argparse
import json
import os

import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import svg_assets as S  # noqa: E402

from PIL import Image  # noqa: E402

# 与 Platforms/Windows/src/theme.rs 的深色主题取值保持一致，
# 否则"预览好看、实机发糊"这类误判又会重来一次。
NAV_BG = (24, 27, 32)
DIM = (139, 148, 160)
TEXT = (223, 228, 235)
ACCENT = (46, 160, 229)
SEL_BG = (37, 42, 50)

ORDER = ["link", "bell", "clipboard", "folder", "media", "blocks", "settings"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size", type=int, default=28)
    ap.add_argument("--ss", type=int, default=4)
    ap.add_argument("--out", default="Temp/svg-preview/nav-at-28.png")
    args = ap.parse_args()

    data = json.load(open("Temp/svg-assets.json", encoding="utf-8"))
    row_h, pad, col_w = args.size + 20, 14, 118
    img = Image.new("RGB", (col_w * 2 + pad * 2, (row_h + 6) * 2 + pad), NAV_BG)

    for state, (icon_color, bg) in enumerate([(DIM, None), (ACCENT, SEL_BG)]):
        y = pad // 2 + state * (row_h + 6)
        for i, name in enumerate(ORDER):
            x = pad + i * col_w
            px, py = x + 6, y + (row_h - args.size) // 2
            glyph = S.render_mask(data[name]["paths24"], cell=args.size, scale=args.ss,
                                  color=icon_color, bg=None)
            if bg:
                slot = Image.new("RGB", (args.size, args.size), bg)
                slot.paste(glyph, (0, 0), glyph)
                img.paste(slot, (px, py))
            else:
                img.paste(glyph, (px, py), glyph)
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    img = img.resize((img.width * 3, img.height * 3), Image.NEAREST)
    img.save(args.out)
    print(f"{args.out}  size={args.size}  ss={args.ss}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
