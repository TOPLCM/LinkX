#!/usr/bin/env python3
"""文件传输真机全链路验收

TCP 通道要双端都绑定成功，端到端的文件传输验收才有前置条件；本脚本把"能发"
变成"有证据地说发对了"：

  1) 双端配对 + 通道绑定（`engine_tcp_bound` / 安卓 `tcp_bound` 必须同时为真）
  2) 逐个发送：小文件 / 中文名 / emoji 名 / 大文件（默认 5 MB，缺失时自动生成）
  3) 每个文件都核对：设备侧 **sha256 与本机逐字节相同**（不是"存在即通过"）
  4) 失败可见性：发一个不存在的路径，必须在 `/state.errors` 里看到可读错误

**本脚本只测"电脑 → 手机"一个方向**；反方向由 `check-phone-to-pc.py` 取证，
两边各测各的，不在这里冒充已验证。

用法：python Scripts/check-file-transfer.py [--big Temp/tx/big-5MB.bin]

前置：Windows 端跑 agent-debug 变体；安卓端已 `adb forward tcp:55700 tcp:55699`。
"""

import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# 仓库根由脚本自身位置推导：脚本不写死任何人的盘符与目录
ROOT = pathlib.Path(__file__).resolve().parents[1]

WIN = "http://127.0.0.1:55699"
AND = "http://127.0.0.1:55700"
DEV_DIR = "/storage/emulated/0/Android/data/com.linkx.app/files/LinkX"


def get(url, timeout=3):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError):
        return None


def post(route, **query):
    url = WIN + route + ("?" + urllib.parse.urlencode(query) if query else "")
    req = urllib.request.Request(url, data=b"", method="POST")
    try:
        with urllib.request.urlopen(req, timeout=8) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"ok": False, "result": str(e)}


def adb(*args):
    # encoding 必须显式给：设备回的是 UTF-8 文件名，而 Windows 上 text 模式默认按
    # 系统代码页解码，中文/emoji 文件名会直接抛 UnicodeDecodeError。
    return subprocess.run(["adb", *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace",
                          env={**os.environ, "MSYS_NO_PATHCONV": "1"})


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def ensure_paired(need_android: bool = True):
    """Windows 是 Central：没连就下发 connect；绑定是异步的，最多等 40 s。

    `need_android=false` 时只看 Windows 侧的 `engine_tcp_bound`——
    交付变体没有控制面可问，但"电脑认为已绑定"本身就要求对端把四步走完。
    """
    for _ in range(20):
        w = get(WIN + "/state")
        if w and w.get("engine_tcp_bound"):
            if not need_android:
                return True
            a = get(AND + "/state") or {}
            if (a.get("engines") or [{}])[0].get("tcp_bound"):
                return True
        if w and not w.get("paired") and w.get("devices"):
            post("/action/connect", addr=w["devices"][-1]["addr"])
        time.sleep(2)
    return False


def wait_win_idle(timeout=90):
    """等 Windows 侧不再有在传任务，并返回这段时间里出现过的错误。"""
    errs = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = get(WIN + "/state") or {}
        for e in s.get("errors") or []:
            if e not in errs:
                errs.append(e)
        if not s.get("has_send") and s.get("pending_send") in (None, ""):
            # 队列空了还要再等一下：完成事件与状态发布之间有一轮 200 ms 的间隔
            time.sleep(1.2)
            s2 = get(WIN + "/state") or {}
            if not s2.get("has_send"):
                return True, s2.get("errors") or []
        time.sleep(1.5)
    return False, errs


def dev_sha(name):
    r = adb("shell", f"sha256sum {DEV_DIR}/{name}")
    out = r.stdout.strip()
    if r.returncode != 0 or not out:
        # 设备侧没有这个文件时把目录列出来，否则报错信息没有诊断价值
        ls = adb("shell", f"ls -la {DEV_DIR}").stdout
        return None, f"设备侧读不到：{r.stderr.strip() or out}\n目录：{ls}"
    return out.split()[0], None


def ensure_fixture(path, size_kb=None, seed_text=None):
    """测试文件一律由脚本自己造：缺文件就只能跳过用例，而全跳过仍能打印"通过"，
    这样的验收不作数。已存在且非空则原样保留（`--big` 指到用户自己的大文件时不能被覆盖）。"""
    if os.path.isfile(path) and os.path.getsize(path) > 0:
        return path
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if seed_text is not None:
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(seed_text)
        return path
    with open(path, "wb") as f:
        blk = os.urandom(1 << 16)
        written = 0
        target = (size_kb or 5120) * 1024
        while written < target:
            chunk = blk[: min(len(blk), target - written)]
            f.write(chunk)
            written += len(chunk)
    return path


def check_one(local, label):
    name = os.path.basename(local)
    want = sha256_file(local)
    # 先把设备侧同名文件删掉：否则上一轮的残留（哈希相同）会让"这条根本没发出去"
    # 判成 PASS —— 文件传输的验收必须有"发送前不存在"这个前提。
    adb("shell", f"rm -f {DEV_DIR}/{name}")
    if dev_sha(name)[0] is not None:
        print(f"  ❌ {label:<12} 设备侧旧文件删不掉，本用例的前提不成立")
        return False
    before = set((get(WIN + "/state") or {}).get("errors") or [])
    r = post("/action/send-file", path=local.replace("\\", "/"))
    ok, errs = wait_win_idle()
    new_errs = [e for e in errs if e not in before]
    # 本机发完 ≠ 设备写完：接收是边写边落盘的，只读一次哈希会撞在半截文件上，
    # 把"还在收尾"误报成"内容不一致"。等到一致或超时为止。
    got, err = dev_sha(name)
    deadline = time.time() + 60
    while got != want and time.time() < deadline:
        time.sleep(2)
        got, err = dev_sha(name)
    good = got == want
    mark = "✅" if good else "❌"
    print(f"  {mark} {label:<12} {name}  {os.path.getsize(local):>9} B")
    if not good:
        print(f"     本机 sha={want[:24]}…")
        print(f"     设备 sha={str(got)[:24]}…  {'已发出但设备没有' if got is None else '内容不一致'}")
        if err:
            print("     " + err.replace(chr(10), chr(10) + "     "))
        if new_errs:
            print(f"     Windows 侧错误：{new_errs[:2]}")
        if not r.get("ok"):
            print(f"     动作回执异常：{r}")
    return good


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--big", default="Temp/tx/big-5MB.bin")
    args = ap.parse_args()

    if get(WIN + "/health") is None:
        print("Windows 控制面不可达", file=sys.stderr)
        return 2
    # 安卓控制面**可选**：交付变体（不带 agent-debug 的 .so）本来就没有它，
    # 而"交付包能不能收文件"恰恰是必须能测的。缺控制面时退化为
    # "PC → 手机 + 设备侧 sha256 核对"，只跳过需要控制面的那几项。
    and_plane = get(AND + "/health") is not None
    if not and_plane:
        print("⚠ 安卓控制面不可达 → 只跑「PC→手机 + 设备侧哈希核对」", file=sys.stderr)
    if not ensure_paired(and_plane):
        print("双端未完成 TCP 通道绑定，后续测的都是别的东西", file=sys.stderr)
        return 3
    print("双端 TCP 已绑定 ✅")

    # 用例文件一律由脚本准备：若"缺文件就 ⚠ 跳过"，四个用例全缺时
    # results 只剩"失败可见"一条 → 打印"通过 1/1"并退出 0，跳过的东西一件没测。
    cases = [
        ("Temp/tx/small.txt", "小文件", {"seed_text": "LinkX 文件传输用例\n"}),
        ("Temp/tx/中文报告.txt", "中文名", {"seed_text": "这是一份中文命名的测试文件。\n" * 32}),
        ("Temp/tx/rocket🚀.txt", "emoji 名", {"seed_text": "rocket 🚀 emoji filename\n"}),
        (args.big, "大文件", {"size_kb": 5120}),
    ]
    results = []
    for f, label, gen in cases:
        ensure_fixture(f, **gen)
        results.append(check_one(f, label))

    # 失败可见性：不存在的路径必须给出**这条**可读错误。
    # 先等发送槽空出来 —— 槽位忙时产品回的是"已有文件正在传输"，那也是一句真话，
    # 但它证明不了"打不开文件"这条路径有没有出声，所以不能算过。
    wait_win_idle()
    before = set((get(WIN + "/state") or {}).get("errors") or [])
    post("/action/send-file", path=str(ROOT / "Temp" / "tx" / "根本不存在.bin"))
    after, deadline = [], time.time() + 20
    while not after and time.time() < deadline:
        time.sleep(1.5)
        after = [e for e in (get(WIN + "/state") or {}).get("errors") or []
                 if e not in before and "打开文件失败" in e]
    visible = bool(after)
    print(f"  {'✅' if visible else '❌'} {'失败可见':<12} 不存在的路径 → {after[:1]}")
    results.append(visible)

    print()
    print(f"通过 {sum(results)}/{len(results)}")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
