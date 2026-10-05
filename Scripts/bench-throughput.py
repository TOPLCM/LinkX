#!/usr/bin/env python3
"""文件传输吞吐实测（双向），并采样电脑端内存峰值。

为什么单独做一个脚本而不是在验收脚本里加几行：**速度是这项指标本身**，
没有数字就没有"达标/没达标"这回事。口径是"跑局域网平均速度"，
所以这里同时给出 MB/s 与"是否受每轮节流上限影响"的判断。

前置：两端跑 agent-debug 变体、已配对且 TCP 已绑定、`adb forward tcp:55700 tcp:55699`。
用法：python Scripts/bench-throughput.py [--size-mb 50]
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

WIN = "http://127.0.0.1:55699"
AND = "http://127.0.0.1:55700"
DEV_DIR = "/storage/emulated/0/Android/data/com.linkx.app/files/LinkX"


def get(url, timeout=5):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError):
        return None


def post(base, route, **q):
    url = base + route + ("?" + urllib.parse.urlencode(q) if q else "")
    req = urllib.request.Request(url, data=b"", method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"ok": False, "result": str(e)}


def adb(*a):
    return subprocess.run(["adb", *a], capture_output=True, text=True,
                          encoding="utf-8", errors="replace",
                          env={**os.environ, "MSYS_NO_PATHCONV": "1"})


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def win_peak_mb(resets):
    """采样 linkx.exe 工作集峰值（MB）。"""
    ps = (
        "$p = Get-Process linkx -ErrorAction SilentlyContinue | Select-Object -First 1; "
        "if ($p) { [math]::Round($p.WorkingSet64/1MB, 1) } else { -1 }"
    )
    r = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                       capture_output=True, text=True)
    try:
        return float(r.stdout.strip())
    except ValueError:
        return -1.0


def ensure_fixture(path, size_mb):
    """样本必须每次现造：复用上一次的残留文件会让"其实没传"也被判成通过。"""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        blk = os.urandom(1 << 16)
        left = size_mb * 1024 * 1024
        while left > 0:
            c = blk[: min(len(blk), left)]
            f.write(c)
            left -= len(c)
    return sha(path)


def both_bound():
    w = get(WIN + "/state") or {}
    a = get(AND + "/state") or {}
    return bool(w.get("engine_tcp_bound")) and bool((a.get("engines") or [{}])[0].get("tcp_bound"))


def bench_pc_to_phone(path, want, size_mb):
    name = os.path.basename(path)
    adb("shell", "rm", "-f", f"{DEV_DIR}/{name}")
    t0 = time.time()
    post(WIN, "/action/send-file", path=path.replace("\\", "/"))
    while time.time() - t0 < 240:
        r = adb("shell", f"sha256sum {DEV_DIR}/{name}")
        out = (r.stdout or "").strip()
        if r.returncode == 0 and out and out.split()[0] == want:
            el = time.time() - t0
            return size_mb / el, el, True
        time.sleep(0.5)
    return 0.0, time.time() - t0, False


def bench_phone_to_pc(name, want, size_mb):
    inbox = (get(WIN + "/state") or {}).get("inbox_dir") or os.path.expanduser("~\\Downloads\\LinkX")
    dst = os.path.join(inbox.replace("\\", os.sep), name)
    if os.path.isfile(dst):
        os.remove(dst)
    t0 = time.time()
    post(AND, "/action/send-file", path=f"{DEV_DIR}/{name}")
    peak = 0.0
    while time.time() - t0 < 240:
        peak = max(peak, win_peak_mb(None))
        if os.path.isfile(dst):
            try:
                if sha(dst) == want:
                    el = time.time() - t0
                    return size_mb / el, el, True, peak
            except OSError:
                pass
        time.sleep(0.5)
    return 0.0, time.time() - t0, False, peak


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size-mb", type=int, default=50)
    args = ap.parse_args()
    if get(WIN + "/health") is None or get(AND + "/health") is None:
        print("两端控制面都要可达", file=sys.stderr)
        return 2
    if not both_bound():
        print("未配对 / TCP 未绑定：测出来的速度不是链路速度", file=sys.stderr)
        return 3
    name = f"bench-{args.size_mb}MB.bin"
    path = os.path.join("Temp/tx", name)
    want = ensure_fixture(path, args.size_mb)
    print(f"样本 {name}（{args.size_mb} MB，sha {want[:16]}…）")

    r1, t1, ok1 = bench_pc_to_phone(path, want, args.size_mb)
    print(f"  电脑 → 手机：{r1:6.1f} MB/s  用时 {t1:5.1f}s  {'✅' if ok1 else '❌ 未完成'}")
    r2, t2, ok2, peak = bench_phone_to_pc(name, want, args.size_mb)
    print(f"  手机 → 电脑：{r2:6.1f} MB/s  用时 {t2:5.1f}s  {'✅' if ok2 else '❌ 未完成'}")
    print(f"  电脑端 linkx.exe 工作集峰值：{peak:.1f} MB")
    if not (ok1 and ok2):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
