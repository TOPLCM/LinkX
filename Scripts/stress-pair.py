#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
stress-pair —— 连续「重启手机 → 电脑端配对」压力复现，用于给崩溃归因提供可重复样本。

为什么需要它：堆损坏（0xc0000374）这类问题的崩溃栈没有参考价值——现场通常在损坏发生
很久之后的无关位置才炸。唯一可信的手段是**同一二进制 + 单一变量开关 + 多轮复现**，
用"活了多少轮"这个计数指标做二分。

用法：
  python Scripts/stress-pair.py --rounds 12 --env LINKX_NO_GATT_SESSION=1
  python Scripts/stress-pair.py --rounds 12                 # 基线（不跳过 GattSession）
  python Scripts/stress-pair.py --exe <path> --cwd <path>   # 默认取仓库内 Target 下的 debug 构建

判读：`died=0` 表示该配置下 N 轮没把进程跑死。样本量小时别当结论，只当方向。
"""
import argparse
import datetime as dt
import os
import pathlib
import subprocess
import sys
import time
import urllib.error
import urllib.request

# 被测二进制默认从脚本自身位置推导（Target 下的 debug 构建），不写死任何人的盘符
ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_EXE = str(ROOT / "Target" / "x86_64-pc-windows-gnu" / "debug" / "linkx.exe")
WIN = "http://127.0.0.1:55699"
AND = "http://127.0.0.1:55700"


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True,
                          text=True, encoding="utf-8", errors="replace", **kw)


def alive():
    try:
        urllib.request.urlopen(WIN + "/health", timeout=3).read()
        return True
    except Exception:
        return False


def launch(env, exe, cwd):
    e = dict(os.environ)
    for kv in env:
        k, _, v = kv.partition("=")
        e[k] = v or "1"
    # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP：脱离本脚本的作业对象，
    # 否则脚本一退出就把被测进程一起带走，测出来的"死亡"全是假的。
    flags = 0x00000008 | 0x00000200
    subprocess.Popen([exe], cwd=cwd, env=e, creationflags=flags,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(30):
        time.sleep(0.5)
        if alive():
            return True
    return False


def wer_crashes(since):
    """数一数指定时刻之后 Windows 记录了几次 linkx 崩溃。"""
    ps = ("$ErrorActionPreference='SilentlyContinue';"
          "$e=Get-WinEvent -FilterHashtable @{LogName='Application';"
          "ProviderName='Application Error'; StartTime=[datetime]'%s'};"
          "@($e | Where-Object { $_.Message -match 'linkx' }).Count"
          % since.strftime("%Y-%m-%d %H:%M:%S"))
    r = sh(["powershell.exe", "-NoProfile", "-Command", ps])
    try:
        return int(r.stdout.strip().splitlines()[-1])
    except Exception:
        return -1


def pair_once():
    """重启手机 → 等广播 → 让电脑端连接 → 等双端 Paired。返回是否成功。"""
    sh(["adb", "shell", "am", "force-stop", "com.linkx.app"])
    time.sleep(2)
    sh(["adb", "shell", "monkey", "-p", "com.linkx.app",
        "-c", "android.intent.category.LAUNCHER", "1"])
    time.sleep(7)
    sh(["adb", "forward", "tcp:55700", "tcp:55699"])
    try:
        d = __import__("json").loads(urllib.request.urlopen(WIN + "/state", timeout=5).read())
        devs = d.get("devices") or []
        if not devs:
            return False, "无设备"
        addr = devs[-1]["addr"]
        req = urllib.request.Request(WIN + "/action/connect?addr=" + addr, data=b"", method="POST")
        urllib.request.urlopen(req, timeout=5).read()
    except Exception as ex:
        return False, f"发起失败 {ex.__class__.__name__}"
    t0 = time.time()
    pair_s = 0.0
    while time.time() - t0 < 45:
        time.sleep(1)
        try:
            w = __import__("json").loads(urllib.request.urlopen(WIN + "/state", timeout=4).read())
            a = __import__("json").loads(urllib.request.urlopen(AND + "/state", timeout=4).read())
            ae = (a.get("engines") or [{}])[0]
            if w.get("paired") and ae.get("paired"):
                # 再等通道绑定：TCP 绑定走完才算局域网数据面真的可用
                pair_s = time.time() - t0
                bound = "TCP:-"
                for _ in range(20):
                    wt = w.get("engine_tcp_bound")
                    at = ae.get("tcp_bound")
                    if wt and at:
                        bound = f"TCP:双端✓ mtu={w.get('ble_mtu')}/{ae.get('ble_mtu')}"
                        break
                    bound = f"TCP:win={wt and '✓' or '✗'}/and={at and '✓' or '✗'} " \
                            f"mtu={w.get('ble_mtu')}/{ae.get('ble_mtu')}"
                    time.sleep(1.5)
                    try:
                        w = __import__("json").loads(
                            urllib.request.urlopen(WIN + "/state", timeout=4).read())
                        a = __import__("json").loads(
                            urllib.request.urlopen(AND + "/state", timeout=4).read())
                        ae = (a.get("engines") or [{}])[0]
                    except Exception:
                        break
                # 配对耗时与等 TCP 绑定的耗时**必须分开报**：合并成一个数字时，
                # "绑定等满超时"会被误读成"配对劣化"，顺着假信号去查 BLE 重试就是白绕一圈。
                return True, (f"配对 {pair_s:.1f}s / 共 {time.time()-t0:.1f}s  {bound}")
        except Exception:
            # 注意：`RemoteDisconnected` 不是 `URLError` 的子类，只捕 URLError
            # 会让整轮压力测试在第 N 轮直接抛栈退出、把已跑出来的数据全丢掉。
            return False, "进程消失"
    return False, "超时"


def main():
    p = argparse.ArgumentParser(description="连续「重启手机 → 电脑端配对」压力复现")
    p.add_argument("--rounds", type=int, default=12)
    p.add_argument("--env", action="append", default=[],
                   help="形如 LINKX_NO_GATT_SESSION=1，可重复")
    p.add_argument("--exe", default=DEFAULT_EXE,
                   help="被测 linkx.exe 路径（默认：仓库内 Target/x86_64-pc-windows-gnu/debug/linkx.exe）")
    p.add_argument("--cwd", default=str(ROOT),
                   help="被测进程的工作目录（默认：仓库根）")
    p.add_argument("--label", default="")
    a = p.parse_args()

    since = dt.datetime.now()
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass

    ok = died = 0
    for i in range(1, a.rounds + 1):
        if not alive():
            print(f"  轮 {i:>2}: 进程不在，重新启动…", flush=True)
            died += 1
            if not launch(a.env, a.exe, a.cwd):
                print(f"  轮 {i:>2}: 重启失败，终止", flush=True)
                break
        r, note = pair_once()
        ok += bool(r)
        print(f"  轮 {i:>2}: {'OK' if r else 'FAIL'}  {note}", flush=True)

    print(f"===== {a.label or ' '.join(a.env) or '基线'}："
          f"{a.rounds} 轮成功 {ok}，进程死亡 {died}，WER 崩溃 {wer_crashes(since)} =====")
    return 0


if __name__ == "__main__":
    sys.exit(main())
