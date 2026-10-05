#!/usr/bin/env python3
"""真机取证：进度条不点击能不能自己走。

三层判据，缺一不可：
  0. 基线：无人输入、没有传输时，屏幕像素必须基本不变。界面任何一帧都在变时，
     "画面在变"这个判据本身就是假的，用它证明"进度条刷新了"没有意义；
  1. 应用状态在推进：读调试面 `/state.host.ui_file_tasks[].percent`；
  2. 屏幕上真的在重画：从屏幕 DC 抠窗口矩形。不用 PrintWindow —— 它会强制目标重画，
     连"忘了 InvalidateRect"都能拍出正确结果，探针就白做了。

用法（先 `cargo run --features agent-debug`，或装带调试面的构建）：
  python Scripts/check-repaint.py baseline      # 只做基线（8 秒）：空闲应 0 变化
  python Scripts/check-repaint.py watch 30      # 观察窗口（秒），需已有传输在跑
"""
import ctypes
import hashlib
import json
import struct
import sys
import time
import urllib.request
from ctypes import wintypes

try:
    ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
except AttributeError:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
CTL = "http://127.0.0.1:55699"
SRCCOPY = 0x00CC0020
DIB_RGB_COLORS = 0
BI_RGB = 0


class RECT(ctypes.Structure):
    _fields_ = [("l", wintypes.LONG), ("t", wintypes.LONG),
                ("r", wintypes.LONG), ("b", wintypes.LONG)]


def state():
    with urllib.request.urlopen(f"{CTL}/state", timeout=5) as r:
        return json.load(r)


def find_linkx():
    EnumWindows = user32.EnumWindows
    GetWindowTextW = user32.GetWindowTextW
    IsWindowVisible = user32.IsWindowVisible
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        if not IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(256)
        GetWindowTextW(hwnd, buf, 256)
        if buf.value.startswith("LinkX"):
            found.append(hwnd)
        return True

    EnumWindows(cb, 0)
    return found[0] if found else None


def grab(hwnd):
    """客户区在屏幕上实际显示的像素 → (bytes, w, h)。

    必须只抠客户区：整窗矩形含 DWM 投影，投影是半透明的，会把窗口背后
    任何变化（桌面时钟、别的窗口的动画）混进来，基线于是永远不为 0。
    """
    cr = RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    w, h = cr.r, cr.b
    tl = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(tl))
    sdc = user32.GetDC(None)
    mem = gdi32.CreateCompatibleDC(sdc)
    bmi = ctypes.create_string_buffer(40)
    struct.pack_into("IiiHHIIiiII", bmi, 0, 40, w, -h, 1, 32, BI_RGB, 0, 0, 0, 0, 0)
    bits = ctypes.c_void_p()
    dib = gdi32.CreateDIBSection(sdc, bmi, DIB_RGB_COLORS, ctypes.byref(bits), None, 0)
    old = gdi32.SelectObject(mem, dib)
    gdi32.BitBlt(mem, 0, 0, w, h, sdc, tl.x, tl.y, SRCCOPY)
    gdi32.GdiFlush()
    data = ctypes.string_at(bits.value, w * h * 4)
    gdi32.SelectObject(mem, old)
    gdi32.DeleteObject(dib)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(None, sdc)
    return data, w, h


def visible_ok(hwnd):
    """确认客户区中心点上方就是 LinkX 自己 —— 否则 BitBlt 拍到的是**盖在它上面的**
    别的窗口，画面永远不变，整个判据就废了。"""
    cr = RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    tl = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(tl))
    pt = wintypes.POINT(tl.x + cr.r // 2, tl.y + cr.b // 2)
    top = user32.WindowFromPoint(pt)
    while top:
        if top == hwnd:
            return True
        owner = ctypes.windll.user32.GetWindow(top, 4)  # GW_OWNER
        if not owner or owner == top:
            break
        top = owner
    return False


def raise_window(hwnd):
    """临时置顶，保证拍到的是 LinkX 自己（测完由调用方取消）。"""
    HWND_TOPMOST = -1
    SWP_NOSIZE, SWP_NOMOVE, SWP_NOACTIVATE = 0x0001, 0x0002, 0x0010
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE)


def lower_window(hwnd):
    HWND_NOTOPMOST = -2
    SWP_NOSIZE, SWP_NOMOVE = 0x0001, 0x0002
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE)


def watch(seconds, label):
    hwnd = find_linkx()
    if not hwnd:
        print("没找到 LinkX 窗口")
        return 1
    raise_window(hwnd)
    time.sleep(0.4)
    if not visible_ok(hwnd):
        print("⚠ 客户区中心被别的窗口盖住，拍到的不是 LinkX —— 本次结果不可信")
    digests, pcts, rows = set(), [], []
    t0 = time.time()
    n = 0
    while time.time() - t0 < seconds:
        st = state()
        tasks = st.get("ui_file_tasks") or []
        live = [t for t in tasks if t.get("percent", 0) < 100]
        p = live[0]["percent"] if live else (tasks[0]["percent"] if tasks else None)
        pcts.append(p)
        data, w, h = grab(hwnd)
        digests.add(hashlib.sha256(data).hexdigest()[:16])
        rows.append((round(time.time() - t0, 1), p, len(digests)))
        n += 1
        time.sleep(0.35)
    lower_window(hwnd)
    print(f"[{label}] {n} 帧 / {len(digests)} 种不同像素 / 百分比取值 {len(set(x for x in pcts if x is not None))} 档")
    for t, p, d in rows[:6]:
        print(f"   t={t:5.1f}s percent={p} 累计不同帧={d}")
    return 0 if rows else 1


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "watch"
    if mode == "baseline":
        rc = watch(8, "基线：无传输")
        st = state()
        print("   当前任务：", st.get("ui_file_tasks"), "battery=", st.get("ui_battery"))
        return rc
    secs = int(sys.argv[2]) if len(sys.argv) > 2 else 30
    rc = watch(secs, "传输中")
    st = state()
    print("   结束时任务：", st.get("ui_file_tasks"))
    return rc


if __name__ == "__main__":
    sys.exit(main())
