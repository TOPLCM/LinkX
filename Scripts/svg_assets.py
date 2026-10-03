#!/usr/bin/env python3
"""SVG 图标包 → 三端资产生成器（唯一真源 = `svg/`）

为什么自己写栅格化：本仓库不引入外部栅格化依赖（inkscape / resvg / cairosvg 都不在工具链里），
而 Pillow 不认 SVG。更关键的是产物不只是位图——

  1. Windows 自绘壳的界面图标必须是**矢量几何**（随 DPI 无损、随主题换色），
     所以导出折线点表（把 C/Q/A 曲线按 ≤6° 采样成多段线），交给 GDI 的
     Path API（`BeginPath`/`PathPolygon`/`FillPath`）按非零环绕规则填充 → 空洞正确。
  2. 安卓用 VectorDrawable，原生就吃 SVG 的 pathData（含 A 弧），直接透传最准。
  3. 应用图标（exe/托盘/MSI/启动器）要位图 → 自带扫描线栅格器生成 PNG/ICO。

统一口径：**所有界面图标同尺寸**，一律归一化到 24×24 逻辑网格，
图形本体居中并撑到统一的安全活动区（3.0..21.0），不留"某个图标就是比别人小一圈"。

用法：
    python Scripts/svg_assets.py                 # 生成全部资产
    python Scripts/svg_assets.py --preview       # 只出预览图到 Temp/svg-preview 供肉眼校验
"""

import argparse
import xml.etree.ElementTree as ET
import json
import math
import os
import re
import struct
import sys
import zlib

NUM_RE = re.compile(r"[-+]?(?:\d+\.\d*|\.\d+|\d+)(?:[eE][-+]?\d+)?")
CMD_RE = re.compile(r"([MmLlHhVvCcSsQqTtAaZz])|([-+]?(?:\d+\.\d*|\.\d+|\d+)(?:[eE][-+]?\d+)?)")

# 图标包文件名 → 内部逻辑名（两端共用）；未登记的 SVG 会被静默跳过，加图标必须改这里
NAME_MAP = {
    "LinkX Logo.svg": "logo",
    # 导航（安卓端命名 ic_nav_*）
    "连接.svg": "link",
    "通知.svg": "bell",
    "剪贴板.svg": "clipboard",
    "文件.svg": "folder",
    "媒体控制.svg": "media",
    "功能.svg": "blocks",
    "设置.svg": "settings",
    "手机.svg": "phone",
    "相册.svg": "album",
    "关于.svg": "info",
    # 操作
    "复制.svg": "copy",
    "发送.svg": "send",
    "上传.svg": "upload",
    "下载.svg": "download",
    "电量.svg": "battery",
    # 媒体播放
    "音乐.svg": "music",
    "播放.svg": "play",
    "停止.svg": "pause",
    "左播放.svg": "prev",
    "右播放.svg": "next",
    "音量大.svg": "vol_up",
    "音量小.svg": "vol_down",
    "随机播放.svg": "shuffle",
    "单曲循环.svg": "repeat",
}
NAV_NAMES = {
    "link", "bell", "clipboard", "folder", "media", "blocks", "settings", "phone", "info",
}
ARC_STEPS_DEG = 6.0          # 圆弧/曲线采样步长
SAFE_BOX = (3.0, 21.0)       # 24 网格内的安全活动区（所有图标一致）

# ---- 光学对齐三参数（24 网格单位；改这里等于改整套图标的观感）----
# 细节下限：短边小于此值的子路径在 26–28px 下不足 1.4px，GDI 填不出洞只能糊成墨点。
# 图标包里"连接"有三个这种碎屑（0.21 / 0.57 / 0.80），正是它发糊的真因。
MIN_FEATURE = 1.0
# 短边下限：横扁/竖长字形在竖排侧栏里会被看成"比别人小一圈"。
# 允许把短边**轻度**拉伸到长边的 80%，超过部分不补（补多了就是明显变形）。
SHORT_RATIO_FLOOR = 0.88
SHORT_BOOST_CAP = 1.10


def parse_path(d):
    """SVG path `d` → 子路径列表，每个是 (x, y) 折线点列（已展开绝对坐标）。"""
    toks = []
    for m in CMD_RE.finditer(d):
        if m.group(1):
            toks.append(("c", m.group(1)))
        else:
            toks.append(("n", float(m.group(2))))
    subs, cur = [], []
    i = 0
    x = y = 0.0
    start_x = start_y = 0.0
    # 上一个绘制命令与其第二个控制点，供 S/T 反射用
    prev_cmd = ""
    prev_c = (0.0, 0.0)

    def flush():
        nonlocal cur
        if len(cur) >= 2:
            subs.append(cur)
        cur = []

    def push(px, py):
        cur.append((px, py))

    while i < len(toks):
        kind, val = toks[i]
        if kind == "c":
            cmd, prev_cmd = val, val
        else:
            cmd = prev_cmd  # 命令字母隐含重复（如 "l 1 2 3 4"）
            kind_n = True
        nums = []
        i += 1
        while i < len(toks) and toks[i][0] == "n":
            nums.append(toks[i][1])
            i += 1
        rel = cmd.islower()
        C = cmd.upper()
        if C == "M":
            flush()
            for k in range(0, len(nums), 2):
                nx, ny = nums[k], nums[k + 1]
                if rel:
                    nx += x
                    ny += y
                x, y = nx, ny
                start_x, start_y = x, y
                push(x, y)
                if k:  # M 之后的隐式段是 L
                    prev_cmd = ("l" if rel else "L")
        elif C == "L":
            for k in range(0, len(nums), 2):
                nx, ny = nums[k], nums[k + 1]
                if rel:
                    nx += x
                    ny += y
                x, y = nx, ny
                push(x, y)
        elif C == "H":
            for v in nums:
                x = x + v if rel else v
                push(x, y)
        elif C == "V":
            for v in nums:
                y = y + v if rel else v
                push(x, y)
        elif C == "C":
            for k in range(0, len(nums), 6):
                c1x, c1y, c2x, c2y, ex, ey = nums[k:k + 6]
                if rel:
                    c1x += x; c1y += y; c2x += x; c2y += y; ex += x; ey += y
                sample_cubic(cur, x, y, c1x, c1y, c2x, c2y, ex, ey)
                prev_c = (c2x, c2y)
                x, y = ex, ey
        elif C == "S":
            for k in range(0, len(nums), 4):
                c2x, c2y, ex, ey = nums[k:k + 4]
                if rel:
                    c2x += x; c2y += y; ex += x; ey += y
                c1x, c1y = (2 * x - prev_c[0], 2 * y - prev_c[1]) if prev_cmd in "sScC" else (x, y)
                sample_cubic(cur, x, y, c1x, c1y, c2x, c2y, ex, ey)
                prev_c = (c2x, c2y)
                x, y = ex, ey
        elif C == "Q":
            for k in range(0, len(nums), 4):
                qx, qy, ex, ey = nums[k:k + 4]
                if rel:
                    qx += x; qy += y; ex += x; ey += y
                # 二次升三次：C1 = P + 2/3(Q-P)，C2 = E + 2/3(Q-E)
                c1x, c1y = x + 2 / 3 * (qx - x), y + 2 / 3 * (qy - y)
                c2x, c2y = ex + 2 / 3 * (qx - ex), ey + 2 / 3 * (qy - ey)
                sample_cubic(cur, x, y, c1x, c1y, c2x, c2y, ex, ey)
                prev_c = (qx, qy)
                x, y = ex, ey
        elif C == "T":
            for k in range(0, len(nums), 2):
                ex, ey = nums[k:k + 2]
                if rel:
                    ex += x; ey += y
                qx, qy = (2 * x - prev_c[0], 2 * y - prev_c[1]) if prev_cmd in "tTqQ" else (x, y)
                c1x, c1y = x + 2 / 3 * (qx - x), y + 2 / 3 * (qy - y)
                c2x, c2y = ex + 2 / 3 * (qx - ex), ey + 2 / 3 * (qy - ey)
                sample_cubic(cur, x, y, c1x, c1y, c2x, c2y, ex, ey)
                prev_c = (qx, qy)
                x, y = ex, ey
        elif C == "A":
            for k in range(0, len(nums), 7):
                rx, ry, rot, large, sweep, ex, ey = nums[k:k + 7]
                if rel:
                    ex += x; ey += y
                sample_arc(cur, x, y, rx, ry, rot, large, sweep, ex, ey)
                x, y = ex, ey
        elif C == "Z":
            if cur:
                push(start_x, start_y)
            flush()
            x, y = start_x, start_y
        prev_cmd = cmd if kind == "c" or C in "MLHVCSQTAZ" else prev_cmd
    flush()
    return subs


def sample_cubic(cur, x0, y0, x1, y1, x2, y2, x3, y3):
    """三次贝塞尔 → 折线。按弦长自适应细分，保证每段 ≤ 0.35 逻辑单位。"""
    span = max(abs(x3 - x0), abs(y3 - y0), 1e-6)
    n = max(8, min(128, int(span * 2.2)))
    for s in range(1, n + 1):
        t = s / n
        mt = 1 - t
        a = mt * mt * mt
        b = 3 * mt * mt * t
        c = 3 * mt * t * t
        d = t * t * t
        cur.append((a * x0 + b * x1 + c * x2 + d * x3, a * y0 + b * y1 + c * y2 + d * y3))


def sample_arc(cur, x0, y0, rx, ry, rot_deg, large, sweep, x1, y1):
    """SVG 弧（端点参数化）→ 中心参数化后按角度采样。"""
    if x0 == x1 and y0 == y1:
        return
    rx, ry = abs(rx), abs(ry)
    if rx == 0 or ry == 0:
        cur.append((x1, y1))
        return
    phi = math.radians(rot_deg)
    cp, sp = math.cos(phi), math.sin(phi)
    dx, dy = (x0 - x1) / 2.0, (y0 - y1) / 2.0
    x1p = cp * dx + sp * dy
    y1p = -sp * dx + cp * dy
    lam = (x1p / rx) ** 2 + (y1p / ry) ** 2
    if lam > 1:  # 半径过小 → 按规范放大
        rx *= math.sqrt(lam)
        ry *= math.sqrt(lam)
    num = max(0.0, rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p)
    den = rx * rx * y1p * y1p + ry * ry * x1p * x1p
    co = math.sqrt(num / den) * (1 if bool(large) != bool(sweep) else -1)
    cxp = co * rx * y1p / ry
    cyp = -co * ry * x1p / rx
    cx = cp * cxp - sp * cyp + (x0 + x1) / 2.0
    cy = sp * cxp + cp * cyp + (y0 + y1) / 2.0

    def ang(ux, uy):
        n = math.hypot(ux, uy) or 1.0
        v = ux / n
        v = max(-1.0, min(1.0, v))
        a = math.acos(v)
        return a if uy >= 0 else -a

    th0 = ang((x1p - cxp) / rx, (y1p - cyp) / ry)
    dth = ang((-x1p - cxp) / rx, (-y1p - cyp) / ry) - th0
    if not sweep and dth > 0:
        dth -= 2 * math.pi
    if sweep and dth < 0:
        dth += 2 * math.pi
    steps = max(2, int(abs(math.degrees(dth)) / ARC_STEPS_DEG) + 1)
    for s in range(1, steps + 1):
        t = th0 + dth * s / steps
        px, py = rx * math.cos(t), ry * math.sin(t)
        cur.append((cp * px - sp * py + cx, sp * px + cp * py + cy))


def load_svg(path):
    raw = open(path, encoding="utf-8").read()
    vb = re.search(r'viewBox="([^"]+)"', raw)
    if not vb:
        raise SystemExit(f"{path}: 没有 viewBox")
    vx, vy, vw, vh = [float(v) for v in vb.group(1).replace(",", " ").split()]
    subs = []
    fills = []
    for m in re.finditer(r"<path\b([^>]*?)/?>", raw):
        attrs = m.group(1)
        d = re.search(r'\bd="([^"]+)"', attrs)
        if not d:
            continue
        f = re.search(r'fill="([^"]+)"', attrs)
        fills.append(f.group(1) if f else "black")
        subs.extend(parse_path(d.group(1)))
    return (vx, vy, vw, vh), subs, fills


def _bbox(subs):
    xs = [p[0] for s in subs for p in s]
    ys = [p[1] for s in subs for p in s]
    if not xs:
        return None
    return min(xs), min(ys), max(xs), max(ys)


def _fit(subs, target=24.0):
    """把子路径集**等比**居中撑满安全活动区（最长边 = avail）。"""
    bb = _bbox(subs)
    if not bb:
        return []
    gx0, gy0, gx1, gy1 = bb
    avail = SAFE_BOX[1] - SAFE_BOX[0]
    k = avail / max(max(gx1 - gx0, 1e-6), max(gy1 - gy0, 1e-6))
    cx, cy = (gx0 + gx1) / 2.0, (gy0 + gy1) / 2.0
    return [
        [(round((px - cx) * k + target / 2.0, 3), round((py - cy) * k + target / 2.0, 3))
         for px, py in s]
        for s in subs
    ]


def normalize(viewbox, subs, target=24.0):
    """任意见习框 → 统一 24 网格，并做**光学对齐**（口径：所有图标同尺寸）。

    三步，缺一不可：

    1. `_fit` 等比撑满：保证最长边一律 18 单位，不会出现"某个图标就是比别人小一圈"。
    2. 细节下限：短边 < MIN_FEATURE 的子路径在 26–28px 下不足 1.4px，填不出洞只会糊成
       墨点 —— 图标包"连接"的 3 个碎屑就是这么把图标毁掉的。直接丢掉，
       并且丢完再 `_fit` 一次（碎屑可能正是撑住外接框的那个点）。
    3. 短边光学下限：横扁字形（"连接" 18×13.9）在**竖排**侧栏里会被看成比别人矮，
       所以把短边轻度拉到长边的 80%。只拉不缩、且封顶 SHORT_BOOST_CAP，
       避免变成明显变形；拉完仍不会越出安全区（短边 < 0.8×18=14.4 才会被拉）。
    """
    out = _fit(subs, target)
    kept = []
    for s in out:
        bb = _bbox([s])
        if not bb:
            continue
        w, h = bb[2] - bb[0], bb[3] - bb[1]
        if min(w, h) >= MIN_FEATURE:
            kept.append(s)
    if not kept:
        return out
    out = _fit(kept, target)

    bb = _bbox(out)
    gx0, gy0, gx1, gy1 = bb
    gw, gh = gx1 - gx0, gy1 - gy0
    long_, short_ = max(gw, gh), min(gw, gh)
    boost = 1.0
    if short_ < SHORT_RATIO_FLOOR * long_:
        boost = min(SHORT_BOOST_CAP, SHORT_RATIO_FLOOR * long_ / max(short_, 1e-6))
    if boost > 1.0:
        # 只在短轴上放大，长轴不动 → 外接框只可能从"偏短"走向"达标"，不会越出安全区
        cy = (gy0 + gy1) / 2.0
        cx = (gx0 + gx1) / 2.0
        out = [
            [((px if gw >= gh else round((px - cx) * boost + cx, 3)),
              (round((py - cy) * boost + cy, 3) if gw >= gh else py))
             for px, py in s]
            for s in out
        ]
    return out


TMPL_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'vector_icon.tmpl')
VECTOR_TMPL = open(TMPL_PATH, encoding='utf-8').read()
LAUNCHER_TMPL = open(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), 'launcher_icon.tmpl'),
    encoding='utf-8',
).read()

def rdp(pts, eps=0.06):
    """Ramer–Douglas–Peucker 抽稀：把采样折线压回最小可表达形状。

    为什么要抽稀：曲线按 6° 采样后一个图标能到 3,800 个点，
    直接塞进 APK 资源与 Rust 常量表都太浪费（且 GDI 逐点连线也白干活）。
    eps=0.06 是 24 网格单位 → 96px 渲染下误差 <0.25px，肉眼看不出。
    """
    if len(pts) < 3:
        return pts
    keep = [False] * len(pts)
    keep[0] = keep[-1] = True
    stack = [(0, len(pts) - 1)]
    while stack:
        a, b = stack.pop()
        if b <= a + 1:
            continue
        (ax, ay), (bx, by) = pts[a], pts[b]
        dx, dy = bx - ax, by - ay
        seg = math.hypot(dx, dy)
        worst, idx = -1.0, -1
        for i in range(a + 1, b):
            px, py = pts[i]
            d = abs((px - ax) * dy - (py - ay) * dx) / seg if seg else math.hypot(px - ax, py - ay)
            if d > worst:
                worst, idx = d, i
        if worst > eps and idx > 0:
            keep[idx] = True
            stack.append((a, idx))
            stack.append((idx, b))
    return [p for p, k in zip(pts, keep) if k]


def to_path_data(paths):
    """折线表 → SVG/VectorDrawable 通用 pathData（全绝对坐标 + Z 闭合）。"""
    out = []
    for sub in paths:
        if not sub:
            continue
        frag = "M" + " L".join(f"{x:.2f},{y:.2f}" for x, y in sub) + "Z"
        out.append(frag)
    return " ".join(out)


def write_ico(png_paths_sizes, out_path):
    """把若干 PNG 打进一个 .ico（Vista+ 允许 ICO 内嵌 PNG，体积小且高分辨率清晰）。"""
    entries = []
    for p, sz in png_paths_sizes:
        data = open(p, "rb").read()
        entries.append((sz, data))
    hdr = struct.pack("<HHH", 0, 1, len(entries))
    off = 6 + 16 * len(entries)
    dirblob = b""
    body = b""
    for sz, data in entries:
        w = 0 if sz >= 256 else sz
        h = 0 if sz >= 256 else sz
        dirblob += struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(data), off)
        off += len(data)
        body += data
    with open(out_path, "wb") as f:
        f.write(hdr + dirblob + body)


def emit_assets(out, args):
    """生成三端资产：安卓 vector / Windows Rust 折线表 / 应用图标 PNG+ICO。"""
    from PIL import Image

    and_dir = "Platforms/Android/app/src/main/res/drawable"
    os.makedirs(and_dir, exist_ok=True)
    # 整表 allow(dead_code)：图标包是两端共用的真源，某端暂时没用其中一个不代表该从表里删
    rust_lines = [
        "#![allow(dead_code)]",
        # 坐标 `3.14f32` 会被 clippy::approx_constant 误判成"想写 PI"，不压掉每次 clippy
        # 都会在这个自动生成的文件上报 error。
        "#![allow(clippy::approx_constant)]",
        # 坐标表类型本身就绕（`&[(&str, &[&[(f32, f32)]])]`），拆 type 别名只会让生成物更难读
        "#![allow(clippy::type_complexity)]",
        "//! 由 `Scripts/svg_assets.py` 从 `svg/` 生成，**不要手改**。",
        "//! 唯一真源 = 图标包；两端共用同一份几何，保证不会出现\"两端图标不一样\"。",
        "",
    ]
    written = []
    for name, rec in out.items():
        if name == "logo":
            continue
        simple = [rdp(s) for s in rec["paths24"]]
        pd = to_path_data(simple)
        # XML lives in Scripts/vector_icon.tmpl (no escaping hazards in a template file)
        vec = VECTOR_TMPL.replace("{SOURCE}", rec["source"]).replace("{PD}", pd)
        fname = (
            f"ic_nav_{name}.xml" if name in NAV_NAMES else f"ic_{name}.xml"
        )
        # 自检：生成的 XML 必须能解析。XML 一旦内联在 Python 字符串里，
        # 一次转义事故就能让全部图标不合法，却要等到 Gradle mergeDebugResources 才炸。
        ET.fromstring(vec)
        with open(os.path.join(and_dir, fname), "w", encoding="utf-8") as f:
            f.write(vec)
        written.append(fname)
        rust_lines.append(f"/// {name}（{rec['source']}）")
        rust_lines.append(f"pub(crate) static {name.upper()}_PATHS: &[&[(f32, f32)]] = &[")
        for sub in simple:
            pts = ", ".join(f"({x:.3}f32, {y:.3}f32)" for x, y in sub)
            rust_lines.append(f"    &[{pts}],")
        rust_lines.append("];")
        rust_lines.append("")
        rec["android"] = fname
        rec["android_points"] = sum(len(s) for s in simple)
    # 全量表：让"每个图标视觉尺寸一致"的断言自动覆盖新增图标，不必手维护清单
    rust_lines.append("/// 全部界面图标（名字 → 折线表）；尺寸一致性断言遍历这张表")
    rust_lines.append(
        "pub(crate) static ALL_ICON_PATHS: &[(&str, &[&[(f32, f32)]])] = &["
    )
    for name, rec in out.items():
        if name == "logo":
            continue
        rust_lines.append(f'    ("{name}", {name.upper()}_PATHS),')
    rust_lines.append("];")
    with open("Platforms/Windows/src/icons_svg.rs", "w", encoding="utf-8") as f:
        f.write("\n".join(rust_lines) + "\n")
    print(f"  安卓 vector：{len(written)} 个；Windows 表：icons_svg.rs")

    # 应用图标：logo 光栅化成多尺寸 PNG + 一个 .ico；安卓另出一份同源 VectorDrawable
    logo = out.get("logo")
    if logo:
        os.makedirs("Temp/ico", exist_ok=True)
        entries = []
        for sz in (16, 24, 32, 48, 64, 128, 256):
            img = render_mask(logo["paths24"], cell=sz, color=(18, 150, 219), bg=None)
            p = os.path.join("Temp/ico", f"linkx-{sz}.png")
            img.save(p)
            entries.append((p, sz))
        write_ico(entries, "Platforms/Windows/LinkX.ico")
        print("  Windows 应用图标：Platforms/Windows/LinkX.ico 已重生成（16..256）")

        # 安卓启动图标：24 网格 → 108dp 画布，网格中心对画布中心。
        # 抽稀会改变轮廓（启动图标要的是品牌形的精确度，不是省字节），所以这里用原始 paths24。
        s = 3.05
        pd = to_path_data(
            [[((x - 12.0) * s + 54.0, (y - 12.0) * s + 54.0) for x, y in sub]
             for sub in logo["paths24"]]
        )
        launcher = (
            LAUNCHER_TMPL.replace("{S}", f"{s}")
            .replace("{BW}", f"{18 * s:.0f}")
            .replace("{PD}", pd)
        )
        ET.fromstring(launcher)
        with open(os.path.join(and_dir, "ic_launcher.xml"), "w", encoding="utf-8") as f:
            f.write(launcher)
        print("  安卓启动图标：res/drawable/ic_launcher.xml 已重生成（与 LinkX.ico 同一份 logo 几何）")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--preview", action="store_true")
    ap.add_argument("--emit", action="store_true", help="生成三端资产（安卓 vector / Windows 表 / ICO）")
    ap.add_argument("--src", default="svg")
    ap.add_argument("--out", default="Temp/svg-preview")
    args = ap.parse_args()

    # 图标包是唯一真源：登记了却缺文件、或丢了文件却没报错，都会让某一端"图标悄悄变回旧的那套"。
    # 新增文件忘了登记同理。两种都当场失败，不要拖到肉眼验收。
    missing = [f for f in NAME_MAP if not os.path.isfile(os.path.join(args.src, f))]
    if missing:
        raise SystemExit(f"NAME_MAP 登记了但图标包里没有：{missing}")
    unregistered = sorted(
        f
        for f in os.listdir(args.src)
        if f.endswith(".svg") and f not in NAME_MAP
    )
    if unregistered:
        raise SystemExit(f"图标包里有未登记的 SVG（不会生成任何资产）：{unregistered}")

    out = {}
    for fname, logical in NAME_MAP.items():
        p = os.path.join(args.src, fname)
        vb, subs, fills = load_svg(p)
        pts = normalize(vb, subs)
        out[logical] = {
            "source": fname,
            "viewBox": list(vb),
            "fills": fills,
            "paths24": pts,
            "point_count": sum(len(s) for s in pts),
        }
        print(f"  {logical:<10} {fname:<18} 子路径 {len(pts):>2}  采样点 {out[logical]['point_count']:>6}")
    os.makedirs("Temp", exist_ok=True)
    with open("Temp/svg-assets.json", "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False)
    print(f"已写 Temp/svg-assets.json（{len(out)} 个图标）")

    if args.preview:
        try:
            from PIL import Image
        except ImportError:
            print("缺 Pillow，无法出预览", file=sys.stderr)
            return 1
        os.makedirs(args.out, exist_ok=True)
        cell, pad = 128, 16
        cols = 4
        rows = (len(out) + cols - 1) // cols
        sheet = Image.new("RGB", (cols * (cell + 2 * pad), rows * (cell + 2 * pad)), (240, 242, 245))
        for idx, (name, rec) in enumerate(out.items()):
            img = render_mask(rec["paths24"], cell=cell, color=(20, 24, 28), bg=None)
            x = (idx % cols) * (cell + 2 * pad) + pad
            y = (idx // cols) * (cell + 2 * pad) + pad
            sheet.paste(img, (x, y), img)
            img.save(os.path.join(args.out, f"{name}.png"))
        sheet.save(os.path.join(args.out, "contact-sheet.png"))
        print(f"预览已出：{args.out}/contact-sheet.png")

    if args.emit:
        emit_assets(out, args)
        with open("Temp/svg-assets.json", "w", encoding="utf-8") as f:
            json.dump(out, f, ensure_ascii=False)
    return 0



def render_mask(paths, cell=96, color=(0, 0, 0), bg=(255, 255, 255), grid=24.0, scale=4):
    """非零环绕规则的扫描线填充 → RGBA 位图（24 网格 → cell 像素，超采样 scale 倍抗锯齿）。"""
    from PIL import Image

    big = cell * scale
    k = big / grid
    polys = [[(px * k, py * k) for px, py in sub] for sub in paths]
    # 归一化：把图形移到画布内（normalize 已居中到 3..21，这里只做裁剪保护）
    rows_px = []
    for sy in range(big):
        yc = sy + 0.5
        xs = []
        for poly in polys:
            n = len(poly)
            for i in range(n):
                x0, y0 = poly[i]
                x1, y1 = poly[(i + 1) % n]
                if y0 == y1:
                    continue
                if (y0 <= yc < y1) or (y1 <= yc < y0):
                    t = (yc - y0) / (y1 - y0)
                    dirn = 1 if y1 > y0 else -1
                    xs.append((x0 + (x1 - x0) * t, dirn))
        if not xs:
            rows_px.append([])
            continue
        xs.sort()
        spans, wind = [], 0
        prev_x = None
        for xv, d in xs:
            if prev_x is not None and xv != prev_x and wind != 0:
                spans.append((prev_x, xv))
            wind += d
            prev_x = xv
        rows_px.append(spans)

    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    px = img.load()
    for sy in range(big):
        for x0, x1 in rows_px[sy]:
            ia, ib = int(max(0, math.floor(x0))), int(min(big - 1, math.ceil(x1)))
            for sx in range(ia, ib + 1):
                px[sx, sy] = (*color, 255)
    img = img.resize((cell, cell), Image.LANCZOS)
    if bg is None:
        return img
    base = Image.new("RGBA", (cell, cell), (*bg, 255))
    base.alpha_composite(img)
    return base



if __name__ == "__main__":
    sys.exit(main())
