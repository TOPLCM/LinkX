#!/usr/bin/env python3
"""Windows 自绘 UI 的快速迭代工具：激活窗口 → 可选点击 → 抓客户区存 PNG。

为什么不用 computer-use 插件：每轮要 list_windows + get_window_state + click 三次调用，
调一次版式就要等三趟。这里直接用 user32/gdi32 一次跑完，输出 PNG 给 Read 看。

注意：`PrintWindow` 会**强制目标重画**，所以本工具只用于"看某一帧长什么样"；
要证明"界面会自己刷新"必须用 `Scripts/check-repaint.py`（从屏幕 DC 抠像素）。
"""
import ctypes
import os
import struct
import subprocess
import sys
import time
import zlib
from ctypes import wintypes

# 必须先声明 DPI 感知，否则 GetClientRect 拿到的是系统虚拟化后的尺寸，
# 抓出来的图会被放大裁切，看着像 UI 画爆了。
try:  # Win10 1703+：SetProcessDpiAwarenessContext 在 user32，不在 shcore
    ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
except AttributeError:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32

SRCCOPY = 0x00CC0020
CAPTUREBLT = 0x40000000
DIB_RGB_COLORS = 0
BI_RGB = 0


def find_window(pid_hint=None):
    EnumWindows = user32.EnumWindows
    GetWindowTextW = user32.GetWindowTextW
    GetWindowThreadProcessId = user32.GetWindowThreadProcessId
    IsWindowVisible = user32.IsWindowVisible
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, lparam):
        if not IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(256)
        GetWindowTextW(hwnd, buf, 256)
        pid = wintypes.DWORD()
        GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if buf.value == "LinkX" and (pid_hint is None or pid.value == pid_hint):
            found.append((hwnd, pid.value))
        return True

    EnumWindows(cb, 0)
    return found


def client_rect(hwnd):
    r = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(r))
    return r


def capture(hwnd, out):
    r = client_rect(hwnd)
    w, h = r.right, r.bottom
    hwnd_dc = user32.GetWindowDC(hwnd)
    mem = gdi32.CreateCompatibleDC(hwnd_dc)
    bmi = ctypes.create_string_buffer(40)
    struct.pack_into("IiiHHIIiiII", bmi, 0, 40, w, -h, 1, 32, BI_RGB, 0, 0, 0, 0, 0)
    bits = ctypes.c_void_p()
    dib = gdi32.CreateDIBSection(hwnd_dc, bmi, DIB_RGB_COLORS, ctypes.byref(bits), None, 0)
    old = gdi32.SelectObject(mem, dib)
    user32.PrintWindow(hwnd, mem, 2)  # PW_RENDERFULLCONTENT
    data = ctypes.string_at(bits.value, w * h * 4)
    gdi32.SelectObject(mem, old)
    gdi32.DeleteObject(dib)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, hwnd_dc)
    # UI_CROP=x,y,w,h（逻辑坐标）+ UI_ZOOM=n：把 20px 的图标放大到能逐点挑毛病
    crop = os.environ.get("UI_CROP")
    zoom = int(os.environ.get("UI_ZOOM", "1"))
    scale = (user32.GetDpiForWindow(hwnd) or 96) / 96.0
    if crop:
        cx, cy, cw, ch = [int(v) for v in crop.split(",")]
        px, py, pw, ph = [round(v * scale) for v in (cx, cy, cw, ch)]
        pw, ph = min(pw, w - px), min(ph, h - py)
        sub = bytearray()
        for row in range(ph):
            off = ((py + row) * w + px) * 4
            sub += data[off : off + pw * 4]
        w, h, data = pw, ph, bytes(sub)
    if zoom > 1:
        big = bytearray()
        for y in range(h):
            line = data[y * w * 4 : (y + 1) * w * 4]
            row = bytearray()
            for x in range(w):
                row += line[x * 4 : (x + 1) * 4] * zoom
            big += row * zoom
        w, h, data = w * zoom, h * zoom, bytes(big)
    write_png(out, w, h, data)
    return w, h


def write_png(path, w, h, bgra):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        row = y * w * 4
        for x in range(w):
            i = row + x * 4
            raw += bytes((bgra[i + 2], bgra[i + 1], bgra[i], 255))
    def chunk(t, d):
        c = t + d
        return struct.pack(">I", len(d)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


def click(hwnd, x, y, scale):
    """按**逻辑坐标**点一下：SendMessage 的 lParam 要的是物理像素，
    系统缩放不是 100% 时不换算就会点到别处去。"""
    px = int(round(x * scale))
    py = int(round(y * scale))
    lp = (py << 16) | (px & 0xFFFF)
    user32.SendMessageW(hwnd, 0x0200, 0, lp)  # WM_MOUSEMOVE
    time.sleep(0.05)
    user32.SendMessageW(hwnd, 0x0201, 1, lp)  # WM_LBUTTONDOWN (MK_LBUTTON)
    time.sleep(0.05)
    user32.SendMessageW(hwnd, 0x0202, 0, lp)  # WM_LBUTTONUP
    time.sleep(0.35)


def main():
    args = sys.argv[1:]
    out = "Temp/ui-shot.png"
    clicks = []
    if "--out" in args:
        i = args.index("--out")
        out = args[i + 1]
        del args[i : i + 2]
    while args:
        clicks.append((int(args[0]), int(args[1])))
        del args[:2]
    wins = find_window()
    if not wins:
        print("no LinkX window")
        return 2
    hwnd, pid = wins[0]
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.4)
    dpi = user32.GetDpiForWindow(hwnd) or 96
    scale = dpi / 96.0
    for x, y in clicks:
        click(hwnd, x, y, scale)
    w, h = capture(hwnd, out)
    print(f"{out} {w}x{h} pid={pid} scale={scale} clicks={clicks}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
