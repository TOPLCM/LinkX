#!/usr/bin/env python3
"""运行期功能开关的内存收益实测

开关的卖点就是"关掉少占内存"，所以这个收益必须被量出来，而不是写在文案里。
三种配置各跑一遍：改开关 → 重启 → 等配对完成 → 连续采样工作集/私有内存 → 取中位数。

口径说明（重要）：
- 被测的是 **agent-debug 变体**（交付的 release 版没有控制面，无法编程化驱动）。
  控制面本身的开销在三种配置里是常量，所以**差值**才是结论，绝对值不是。
- 必须等到 `Paired` 才采样：UDP 发现与 TCP 服务端只在配对后创建（`ensure_channels`），
  未配对时三种配置都一样，量不出差别。

用法：python Scripts/measure-feature-memory.py [--samples 10]
"""

import argparse
import json
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request

WIN = "http://127.0.0.1:55699"
MODULES = ("notifications", "clipboard", "file_transfer")

# 名称 -> 三种配置的开关取值（1=开）
CONFIGS = {
    "全开（基线）": (1, 1, 1),
    "关文件传输": (1, 1, 0),
    "只留通知": (1, 0, 0),
    "全关": (0, 0, 0),
}
# 报告同时写文件：后台运行时 shell 重定向可能把 stdout 整个丢掉，光靠 stdout 不可靠
REPORT = "Temp/feat-mem-report.txt"


def get(url, timeout=3):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError):
        return None


def post(url, timeout=5):
    req = urllib.request.Request(url, data=b"", method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"ok": False, "error": str(e)}


def pid_of_linkx():
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         "(Get-Process linkx -ErrorAction SilentlyContinue | Select-Object -First 1).Id"],
        capture_output=True, text=True).stdout.strip()
    return int(out) if out.isdigit() else None


def mem_bytes(pid):
    """工作集 / 私有内存 / 线程数 / 句柄数。进程没了返回 None。

    线程与句柄一起看才有意义：本产品的模块成本主要不在内存，而在
    "多起几条线程、多开几个 socket"（UDP 发现线程、TCP accept 与每连接的 rx 线程）。
    """
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         f"$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue;"
         "if($p){'{0} {1} {2} {3}' -f $p.WorkingSet64,$p.PrivateMemorySize64,"
         "$p.Threads.Count,$p.HandleCount}"],
        capture_output=True, text=True).stdout.split()
    if len(out) != 4:
        return None
    return int(out[0]), int(out[1]), int(out[2]), int(out[3])


def reconnect(timeout=30):
    """重启后引擎是空的，必须重新下发连接（与用户在连接页点设备是同一条路径）。

    地址是安卓的随机 RPA，每次重启都可能变 → 从 `/state` 现读，绝不写死。
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        st = get(WIN + "/state")
        devs = (st or {}).get("devices") or []
        if devs:
            addr = devs[-1]["addr"]
            post(f"{WIN}/action/connect?addr={addr}")
            return addr
        time.sleep(1.5)
    return None


def wait_phase(pid, samples, settle=1.0):
    """等配对，然后连续采样。返回 (工作集MB, 私有MB, 线程数, 句柄数, 采样数)。"""
    deadline = time.time() + 60
    while time.time() < deadline:
        st = get(WIN + "/state")
        if st and st.get("paired"):
            break
        time.sleep(1.5)
    else:
        return None, None, None, None, 0
    # 配对后 TCP/UDP 通道与缓冲是逐步建起来的，先让它静置几轮再采
    time.sleep(4)
    ws, priv, thr, hnd = [], [], [], []
    for _ in range(samples):
        m = mem_bytes(pid)
        if m is None:
            break
        ws.append(m[0])
        priv.append(m[1])
        thr.append(m[2])
        hnd.append(m[3])
        time.sleep(settle)
    if not ws:
        return None, None, None, None, 0
    return (
        statistics.median(ws) / 1024 / 1024,
        statistics.median(priv) / 1024 / 1024,
        statistics.median(thr),
        statistics.median(hnd),
        len(ws),
    )


def apply_config(flags, restart_wait=6):
    for name, val in zip(MODULES, flags):
        r = post(f"{WIN}/action/feature?module={name}&on={val}")
        if not r or not r.get("ok"):
            # 已经是目标值时动作也会成功，所以这里只可能真的是失败了
            print(f"  ! 设置 {name}={val} 失败：{r}", file=sys.stderr)
            return None
    old = pid_of_linkx()
    post(WIN + "/action/restart")
    deadline = time.time() + restart_wait + 25
    while time.time() < deadline:
        time.sleep(1.5)
        new = pid_of_linkx()
        if new and new != old:
            return new
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--samples", type=int, default=8)
    args = ap.parse_args()

    if get(WIN + "/health") is None:
        print("Windows 端控制面不可达（先起 linkx.exe --features agent-debug）", file=sys.stderr)
        return 2

    rows = []
    base_ws = base_priv = base_thr = base_hnd = None
    for label, flags in CONFIGS.items():
        print(f"== {label} {dict(zip(MODULES, flags))}")
        pid = apply_config(flags)
        if not pid:
            print("  重启后没起来，终止", file=sys.stderr)
            return 3
        addr = reconnect()
        if not addr:
            print("  扫不到对端设备，终止", file=sys.stderr)
            return 5
        st = get(WIN + "/state") or {}
        loaded = st.get("features_loaded") or []
        # 加载结果必须断言，不能只打印：配错门控时"报告照样出"就是自欺
        want = {MODULES[i]: bool(f) for i, f in enumerate(flags)}
        got = set(st.get("features_wanted") or [])
        bad = [k for k, v in want.items() if (k in got) != v]
        if bad:
            print(f"  ！开关未落到 wanted：{bad}", file=sys.stderr)
            return 6
        ws, priv, thr, hnd, n = wait_phase(pid, args.samples)
        if ws is None:
            print("  未配对或采样失败，终止", file=sys.stderr)
            return 4
        if base_ws is None:
            base_ws, base_priv, base_thr, base_hnd = ws, priv, thr, hnd
        rows.append((label, flags, ws, priv, thr, hnd,
                     ws - base_ws, priv - base_priv, thr - base_thr, hnd - base_hnd, n))
        print(f"  已加载={loaded}  工作集={ws:.1f}MB  线程={thr:.0f}  句柄={hnd:.0f}  (n={n})")

    header = (f"{'配置':<14} {'通/贴/文':<9} {'工作集MB':>8} {'Δ工作集':>7} "
              f"{'私有MB':>7} {'Δ私有':>6} {'线程':>4} {'Δ线程':>5} {'句柄':>5} {'Δ句柄':>5}")
    lines = [
        f"{label:<14} {'/'.join(str(f) for f in flags):<9} "
        f"{ws:>8.1f} {dws:>7.1f} {priv:>7.1f} {dpriv:>6.1f} "
        f"{thr:>4.0f} {dthr:>5.0f} {hnd:>5.0f} {dhnd:>5.0f}"
        for (label, flags, ws, priv, thr, hnd, dws, dpriv, dthr, dhnd, _n) in rows
    ]
    report = chr(10).join([header] + lines) + chr(10)
    print()
    print(report, end="")
    # 报告同时写文件：后台运行时 shell 重定向可能把 stdout 整个丢掉
    with open(REPORT, "w", encoding="utf-8") as f:
        f.write(report)
    print(f"（同一份表格已写入 {REPORT}）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
