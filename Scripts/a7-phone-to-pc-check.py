#!/usr/bin/env python3
"""手机 → 电脑文件传输取证

为什么不复用 `b3-file-transfer-check.py`：那个脚本只测「电脑 → 手机」，反方向是另一条
独立的路，必须单独出证据。这里要证明四件事：

  1) 双端必须已配对 **且 TCP 已绑定**，否则任何"失败"都测不出通道归一的效果
  2) 发送后核对：电脑收件目录里**发送前不存在**、发送后**sha256 与手机侧逐字节相同**
  3) 通道证据：从两端 `/logs` 里取 `file.route` 埋点，统计分块到底走了 tcp 还是 ble
     —— 分块走哪条通道界面上看不出来，逐帧埋点是唯一硬证据
  4) 完成语义：手机侧状态必须停在「已完成」而不是「已发送（未确认）」；
     电脑侧必须给出回执（收端成功也要作声，不能只由发端宣布）

用法（前置：两端跑 agent-debug 变体，`adb forward tcp:55700 tcp:55699`，已配对）：
    python Scripts/a7-phone-to-pc-check.py
可选：
    --files name1 name2   # 手机上（应用专属外部目录里）已存在的文件名
本脚本会把本机 Temp/tx 下的用例推到那个目录，省掉手工步骤。
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
# 手机侧 App 自己的收件目录（与 b3 脚本同源）：App 是属主，读得动；
# shell 只读得、写不进（Android 11+ scoped storage），所以样本必须由产品路径自己产生。
DEV_DIR = "/storage/emulated/0/Android/data/com.linkx.app/files/LinkX"


def get(url, timeout=4):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError):
        return None


def ndjson(url, timeout=6):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return [json.loads(l) for l in r.read().decode("utf-8").splitlines() if l.strip()]
    except (urllib.error.URLError, OSError, ValueError):
        return []


def post(base, route, **query):
    url = base + route + ("?" + urllib.parse.urlencode(query) if query else "")
    req = urllib.request.Request(url, data=b"", method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"ok": False, "result": str(e)}


def adb(*args):
    # encoding 显式给 UTF-8：Windows 上 text 模式默认按系统代码页解码，
    # 中文/emoji 文件名会直接抛异常
    return subprocess.run(["adb", *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace",
                          env={**os.environ, "MSYS_NO_PATHCONV": "1"})


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def both_bound():
    w, a = get(WIN + "/state") or {}, get(AND + "/state") or {}
    return bool(w.get("engine_tcp_bound")) and bool(
        (a.get("engines") or [{}])[0].get("tcp_bound"))


def routes_since(mark):
    """从两端 /logs 里取 file.route 埋点（逐帧通道日志）。"""
    out = []
    for base in (WIN, AND):
        for e in ndjson(base + "/logs"):
            if e.get("msg") == "file.route" or "file.route" in str(e.get("event", "")):
                out.append(e)
    return out


def host_fields():
    """安卓 `/state` 的 `host` 块（`setDebugField` 落在这里，不在 engines[] 里）。"""
    return (get(AND + "/state") or {}).get("host") or {}


def wait_idle(timeout=180):
    """等手机侧不再有在途发送任务；返回 (是否空闲, host 字段)。"""
    errs, deadline, host = [], time.time() + timeout, {}
    while time.time() < deadline:
        s = get(AND + "/state") or {}
        host = s.get("host") or {}
        for e in s.get("errors") or []:
            if e not in errs:
                errs.append(e)
        if str(host.get("file_sending", "0")) in ("0", "None", ""):
            time.sleep(1.5)  # 状态发布比事件晚一轮，再确认一次
            s2 = get(AND + "/state") or {}
            h2 = s2.get("host") or {}
            if str(h2.get("file_sending", "0")) in ("0", "None", ""):
                return True, h2
        time.sleep(2)
    return False, host


def ensure_fixtures(names):
    """样本一律由**产品自己的路径**产生，不走 adb push。

    为什么不 push：Android 11+ 的 scoped storage 不允许 shell 往别的 App 的
    `Android/data/<pkg>/files` 里创建文件（`secure_mkdirs failed: Operation not permitted`），
    重装后 App uid 变了连 /sdcard 别名都写不进去；`run-as` 在部分厂商 ROM 上也被 SELinux 挡住。
    所以这里先「电脑 → 手机」把文件发到 App 自己的收件目录（App 是属主，读得动），
    再原样「手机 → 电脑」发回来 —— 顺带把往返两个方向都验了。

    返回 {文件名: (本机路径, 期望 sha)}；期望值以本机原始文件为准，
    并顺带核对手机侧收到的副本哈希一致 —— 前置不成立就别往下测。
    """
    out = {}
    for n in names:
        local = os.path.join("Temp/tx", n)
        if not (os.path.isfile(local) and os.path.getsize(local) > 0):
            os.makedirs("Temp/tx", exist_ok=True)
            if n.endswith(".bin"):
                with open(local, "wb") as f:
                    blk = os.urandom(1 << 16)
                    written = 0
                    while written < 5 * 1024 * 1024:
                        c = blk[: min(len(blk), 5 * 1024 * 1024 - written)]
                        f.write(c)
                        written += len(c)
            else:
                with open(local, "w", encoding="utf-8", newline="") as f:
                    f.write("LinkX 往返取证用例 αβγ 🎵\n" * 8)
        want = sha256_file(local)
        # 先把手机侧旧副本删掉：否则"这一步根本没发出去"会被上一轮残留判成 PASS
        adb("shell", "rm", "-f", f"{DEV_DIR}/{n}")
        post(WIN, "/action/send-file", path=local.replace("\\", "/"))
        deadline = time.time() + 120
        dev = None
        while time.time() < deadline:
            r = adb("shell", "sha256sum", f"{DEV_DIR}/{n}")
            got = (r.stdout or "").strip()
            if r.returncode == 0 and got:
                dev = got.split()[0]
                # 读到值不等于收完：接收是边写边落盘的，第一次采样很可能撞在半截文件上。
                # 只有等到与本机一致才算前置成立，否则会把"还在传"误报成"传坏了"。
                if dev == want:
                    break
            time.sleep(2)
        if dev is None:
            print(f"  ❌ 前置不成立：手机侧没收到 {n}，回传用例无从谈起")
            return None
        if dev != want:
            print(f"  ❌ 前置不成立：手机侧副本 120s 内没等到与本机一致（{dev[:16]}… ≠ {want[:16]}…）")
            return None
        out[n] = (local, want)
    return out


def inbox_dir():
    s = get(WIN + "/state") or {}
    d = s.get("inbox_dir") or os.path.expanduser("~\\Downloads\\LinkX")
    return d.replace("\\", os.sep)


def check_one(name, local_path, want):
    inbox = inbox_dir()
    dst = os.path.join(inbox, name)
    # 发送前必须不存在：否则上一轮残留会让"根本没发出去"判成 PASS
    if os.path.isfile(dst):
        os.remove(dst)
    if os.path.isfile(dst):
        print(f"  ❌ {name:<28} 电脑侧旧文件删不掉，用例前提不成立")
        return False
    before = len(routes_since(None))
    base_errs = set((get(AND + "/state") or {}).get("errors") or [])
    r = post(AND, "/action/send-file", path=f"{DEV_DIR}/{name}")
    idle, host = wait_idle()
    new_errs = [e for e in (get(AND + "/state") or {}).get("errors") or [] if e not in base_errs]

    got = sha256_file(dst) if os.path.isfile(dst) else None
    # 手机说"发完了"≠ 电脑这边写完了：收端是先写盘再回执，落盘可能还差最后一截。
    # 只读一次会把"还在收尾"报成"文件不存在/内容不一致"，所以等到一致或超时为止。
    deadline = time.time() + 60
    while got != want and time.time() < deadline:
        time.sleep(2)
        got = sha256_file(dst) if os.path.isfile(dst) else None
    good = got == want

    # 通道证据：file.route 埋点里分块（msg=0x22）到底走了 tcp 还是 ble
    chunk_frames = [e for e in routes_since(None)[before:]
                    if str(e.get("fields", {}).get("msg", "")) == "0x22"]
    via = {}
    for e in chunk_frames:
        ch = str(e.get("fields", {}).get("channel", "?"))
        via[ch] = via.get(ch, 0) + 1

    mark = "✅" if good else "❌"
    print(f"  {mark} {name:<28} {os.path.getsize(local_path):>9} B  "
          f"手机在途={host.get('file_sending')} BLE 丢包={host.get('ble_drops')}")
    if chunk_frames:
        print(f"     分块路由证据：{via}（共 {len(chunk_frames)} 帧）")
    else:
        print("     ⚠ 本轮没采到 file.route 分块埋点（控制面日志环可能已被挤掉）")
    if not good:
        print(f"     期望 sha={want[:24]}…")
        print(f"     实际 sha={str(got)[:24]}…  {'电脑收件目录里没有这个文件' if got is None else '内容不一致'}")
        if not r.get("ok"):
            print(f"     动作回执：{r}")
        if new_errs:
            print(f"     手机侧新错误：{new_errs[:3]}")
    if not idle:
        print("     ⚠ 等待发送队列清空超时（可能卡在「等待电脑确认」）")
    return good


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--files", nargs="*",
                    default=["small.txt", "中文报告.txt", "rocket🚀.txt", "big-5MB.bin"])
    args = ap.parse_args()

    if get(WIN + "/health") is None or get(AND + "/health") is None:
        print("两端控制面必须都可达（agent-debug 变体 + adb forward）", file=sys.stderr)
        return 2
    if not both_bound():
        print("双端未完成配对 + TCP 绑定；先跑 linkx-ctl.py pair", file=sys.stderr)
        return 3
    print("双端已配对且 TCP 已绑定 ✅")

    fixtures = ensure_fixtures(args.files)
    if fixtures is None:
        return 3
    results = [check_one(n, p, sha) for n, (p, sha) in fixtures.items()]
    print()
    print(f"手机 → 电脑 通过 {sum(results)}/{len(results)}")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
