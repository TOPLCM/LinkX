#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""传输压力/浸泡测试：把手机↔电脑两个方向反复跑，逐轮核对，任何一轮不成立都记下来。

设计要点：
- **只认"两端都说完话"的结论**：界面上 `已完成` 且字节数与声明一致才算过；
  任何 `失败 / 校验失败 / 摘要不符 / 未校验` 都算挂。
- **每轮都量工作集**：内存红线要的是"反复跑不涨"，单次读数说明不了什么。
- **收尾扫埋点**：`orphan_chunk` / `file.late_frame` / `tcp.rx.backpressure` 这些
  不一定会变成界面上的话，但它们是"某一端正在悄悄丢东西"的前兆。

用法：
  python Scripts/soak-transfer.py [轮数] [--phone-ids 1,2,3] [--device-name 子串]

手机侧相册条目与目标设备默认**自动发现**（清单见 `--help`），也可以显式指定。
"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import urllib.parse
import urllib.request

# 仓库根由脚本自身位置推导；中间产物一律进 Temp/
ROOT = pathlib.Path(__file__).resolve().parents[1]
WIN = "http://127.0.0.1:55699"
OUT = str(ROOT / "Temp" / "soak")
os.makedirs(OUT, exist_ok=True)

# 电脑发出去的文件尺寸（字节）：覆盖"小得离谱"到"越过丢帧保护线"
PC_SIZES = [1023, 200 * 1024, 5 * 1024 * 1024, 30 * 1024 * 1024, 100 * 1024 * 1024]


# "等待对端确认"不是终态：那是"分块已交出去、还没收到对端的 FILE_DONE 回执"，
# 几百毫秒后就会变成"已完成"；把它当终态会把成功读成失败。
NON_TERMINAL = (
    "接收中",
    "发送中",
    "重传中",
    "传输中",
    "已发完（结束帧未发出）",
    "等待对端确认",
)


def state():
    return json.load(urllib.request.urlopen(WIN + "/state", timeout=8))


def action(name, **kw):
    q = urllib.parse.urlencode(kw)
    req = urllib.request.Request(f"{WIN}/action/{name}?{q}", method="POST")
    return json.load(urllib.request.urlopen(req, timeout=15))


def tasks():
    s = state()
    return s.get("ui_file_tasks") or []


def wait_done(name, direction, timeout=300):
    """等这一条任务落到终态（见 `NON_TERMINAL`）。返回 (状态, 百分比, 声明字节)。"""
    t0 = time.time()
    while time.time() - t0 < timeout:
        for t in tasks():
            if t["name"] == name and t["dir"] == direction:
                if t["state"] not in NON_TERMINAL:
                    return t["state"], t.get("percent", -1), t.get("size", 0)
        time.sleep(1.0)
    return "TIMEOUT", -1, 0


def wait_for(predicate, timeout, interval=1.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        v = predicate()
        if v:
            return v, time.time() - t0
        time.sleep(interval)
    return None, time.time() - t0


def ndjson_events(names, since_ms):
    """读电脑端埋点（追加写，取尾部即可）。判"这一轮到底成没成"只该问它，不该问界面行。"""
    path = os.path.join(os.environ["APPDATA"], "LinkX", "Logs", "linkx-debug.ndjson")
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as f:
            f.seek(max(0, size - 2_000_000))
            chunk = f.read().decode("utf-8", "replace")
    except OSError:
        return []
    out = []
    for line in chunk.splitlines():
        if not line.startswith("{"):
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("event") in names and e.get("ts_ms", 0) >= since_ms:
            out.append((e["ts_ms"], e.get("fields") or {}))
    return out


PHONE_PLANE = "http://127.0.0.1:55700"


def phone_done(name):
    """手机侧调试面的 `file_rows` 是一条拼出来的字符串（`名字#fileId=状态`），取这条文件的末态。

    手机没装带控制面的包时读数会失败——那只降级成"少一个证人"，不许把整轮判定带崩。
    """
    try:
        d = json.loads(urllib.request.urlopen(PHONE_PLANE + "/state", timeout=8).read().decode("utf-8"))
        rows = str((d.get("host") or {}).get("file_rows") or "")
        hit = next((seg for seg in rows.split(",") if name in seg), None)
        return hit.split("=")[-1] if hit else "无此行"
    except Exception as e:  # noqa: BLE001
        return f"读数失败:{type(e).__name__}"


def ws_mb():
    s = state()
    return s.get("mem_working_set_mb", 0.0)


def make_pc_file(size):
    p = os.path.join(OUT, f"pc-{size}.bin")
    if not os.path.exists(p) or os.path.getsize(p) != size:
        with open(p, "wb") as f:
            written = 0
            block = bytes((i * 7 + 13) & 0xFF for i in range(65536))
            while written < size:
                n = min(len(block), size - written)
                f.write(block[:n])
                written += n
    return p


def pick_device(name_filter):
    """返回 (候选设备列表, 失败原因)。

    扫描器要时间：刚启动的实例设备表是空的，直接判"没有手机"是假失败，所以要等。
    默认不按机型过滤——设备表按发现顺序追加，最后一条最可能就是当前这台手机；
    同时挂着好几台设备时才用 `--device-name` 收窄。失败必须带原因，不能只回"没有"。
    """
    devs = []
    for _ in range(60):
        devs = state().get("devices", []) or []
        if name_filter:
            devs = [d for d in devs if name_filter in str(d.get("name"))]
        if devs:
            return devs, None
        time.sleep(2)
    why = ("设备表等了 120 秒仍为空：手机侧 LinkX 没在广播？先 am start 拉起来"
           if not name_filter else
           f"设备表里没有名字含 {name_filter!r} 的设备（现有：{[d.get('name') for d in devs][:8]}）")
    return [], why


def discover_phone_ids(want):
    """自动取手机相册条目的 id：`/action/album-list?page=0` → 轮询 `/state.ui_album_items`。

    `page` 从 **0** 起算（从 1 起步会拿到空清单）；清单是异步回填的，下发后要轮询才拿得到。
    返回 (id 列表, 失败原因)。
    """
    ids = []
    action("album-list", page=0, per=max(want, 24))
    deadline = time.time() + 40
    while time.time() < deadline:
        items = state().get("ui_album_items") or []
        ids = [int(it["id"]) for it in items if it.get("id") is not None][:want]
        if len(ids) >= want:
            return ids, None
        time.sleep(1)
    return ids, (f"相册清单只拿到 {len(ids)} 条（想要 {want} 条）："
                 "手机侧相册页可能没加载出来；可先用 linkx-ctl.py action win album-list page=0 复核，"
                 "或用 --phone-ids 显式指定")


def parse_args():
    ap = argparse.ArgumentParser(description="手机↔电脑双向传输压力/浸泡测试")
    ap.add_argument("rounds", nargs="?", type=int, default=3, help="轮数（默认 3）")
    ap.add_argument("--phone-ids", default="",
                    help="逗号分隔的相册条目 id；默认留空 = 自动从相册清单发现")
    ap.add_argument("--phone-id-count", type=int, default=6,
                    help="自动发现时取前 N 条相册条目（默认 6）")
    ap.add_argument("--device-name", default="",
                    help="按名字子串过滤设备表；默认留空 = 自动取设备表最后一条（最近发现的设备）")
    return ap.parse_args()


def main():
    args = parse_args()
    rounds = args.rounds
    devs, why = pick_device(args.device_name)
    if not devs:
        print(f"NO PHONE IN LIST —— {why}")
        return 1
    addr = devs[-1]["addr"]
    action("connect", addr=addr)
    time.sleep(25)
    s = state()
    if not s.get("paired"):
        print("NOT PAIRED", s.get("phase"))
        return 1
    print(f"paired, tcp_bound={s.get('engine_tcp_bound')}, 目标设备={devs[-1].get('name')}")

    if args.phone_ids.strip():
        phone_ids = [int(x) for x in re.split(r"[,\s]+", args.phone_ids.strip()) if x]
    else:
        phone_ids, why = discover_phone_ids(args.phone_id_count)
        if not phone_ids:
            print(f"PHONE IDS 自动发现失败 —— {why}")
            return 1
        if len(phone_ids) < args.phone_id_count:
            print(f"⚠ {why} —— 就用这 {len(phone_ids)} 条跑")
    print(f"相册条目 {len(phone_ids)} 条：{phone_ids}")

    fails, peak = [], 0.0
    for r in range(1, rounds + 1):
        # --- 手机 → 电脑（相册导出，走 FILE_* 通道）---
        for i in phone_ids:
            d = os.path.join(OUT, f"r{r}-{i}")
            os.makedirs(d, exist_ok=True)
            t0 = int(time.time() * 1000)
            action("album-export", id=i, dir=d.replace("\\", "/"))
            # 判据只认埋点里这一对：`file.meta`（声明 size）+ 同 file_id 的 `file.done`(ok=true)。
            # 不看任务行——行是按 (方向, 名字) 复用的，上一轮的同名终态行会一直躺在那儿，
            # 靠它判"安静不安静"会在这一轮还没起播时就误判，然后把只写了一半的文件读成成功。
            meta = None
            deadline = time.time() + 60
            while meta is None and time.time() < deadline:
                meta = next((e for e in ndjson_events(("file.meta",), t0)), None)
                if meta is None:
                    time.sleep(0.5)
            verdict, got, newest = "NO-META", 0, None
            if meta:
                fid, declared = meta[1].get("file_id"), int(meta[1].get("size", "0"))
                done, _ = wait_for(
                    lambda: next(
                        (e for e in ndjson_events(("file.done",), t0) if e[1].get("file_id") == fid),
                        None,
                    ),
                    420,
                )
                # done 之后还要等文件真的落定：收端是先写盘再回执，但目录读数仍可能抢先
                time.sleep(1.5)
                files = [f for f in os.listdir(d)]
                newest = max(files, key=lambda f: os.path.getsize(os.path.join(d, f))) if files else None
                got = sum(os.path.getsize(os.path.join(d, f)) for f in files)
                if not done:
                    verdict = "NO-DONE"
                elif done[1].get("ok") != "true":
                    verdict = "PEER-FAILED"
                elif got != declared:
                    verdict = f"TRUNCATED({got}/{declared})"
                else:
                    verdict = "OK"
            peak = max(peak, ws_mb())
            print(f"r{r} phone→pc id={i:<11} {verdict:22} {got:>11} B  {newest}  ws={ws_mb():.1f} MB")
            if verdict != "OK":
                fails.append(f"r{r} phone→pc id={i} {verdict}")
            shutil_rmtree(d)

        # --- 电脑 → 手机 ---
        for size in PC_SIZES:
            p = make_pc_file(size)
            name = os.path.basename(p)
            # 发送槽位一次只容一条：上一轮的行还挂在途时，这一轮的请求会被
            # "已有文件正在传输"直接拒掉，界面上连行都不新增——等待方于是读到 TIMEOUT，
            # 看着像传输坏了，其实是排队没排明白。先等槽位空出来。
            free, _ = wait_for(
                lambda: None
                if any(t["dir"] == "send" and t["state"] in NON_TERMINAL for t in tasks())
                else True,
                180,
            )
            if not free:
                fails.append(f"r{r} pc→phone {size} 发送槽位 180s 没空出来")
                continue
            t0 = int(time.time() * 1000)
            action("send-file", path=p.replace("\\", "/"))
            # 判据问埋点，不问界面行。界面行是按名字复用的：小文件经常在一个轮询间隔内
            # 就从无到有直接落到"已完成"，等"在途行"会把这种正常结局读成"没起播"。
            meta, _ = wait_for(
                lambda: next(
                    (e for e in ndjson_events(("file.meta.send",), t0)
                     if e[1].get("size") == str(size)),
                    None,
                ),
                60,
            )
            if meta is None:
                fails.append(f"r{r} pc→phone {size} 没起播（发送被拒？）")
                continue
            fid = meta[1].get("file_id")
            done, _ = wait_for(
                lambda: next(
                    (e for e in ndjson_events(("file.done.send",), t0)
                     if e[1].get("file_id") == fid and e[1].get("ok") == "true"
                     and e[1].get("cancelled") != "true"),
                    None,
                ),
                420,
            )
            st, pct, declared = wait_done(name, "send", timeout=60)
            peak = max(peak, ws_mb())
            # 第二个证人：界面行与手机自己那行。电脑端"已完成"靠手机的回执，
            # 但回执只证明"它收全并校验过了"，不证明文件交到了用户会看的地方。
            print(
                f"r{r} pc→phone {name:<24} 埋点done={'OK' if done else 'NO-DONE'}  "
                f"{st:<10} {pct}%  手机侧={phone_done(name)}  ws={ws_mb():.1f} MB"
            )
            if done is None:
                fails.append(f"r{r} pc→phone {size} 埋点里没有 ok=true 的 file.done.send")
            elif st not in ("已完成", "已发送（未确认）") or pct != 100:
                fails.append(f"r{r} pc→phone {size} -> {st} {pct}%")

    print(f"\npeak working set {peak:.1f} MB")
    errs = state().get("errors") or []
    print("UI errors:", json.dumps(errs[-6:], ensure_ascii=False))

    log = os.path.join(os.environ["APPDATA"], "LinkX", "Logs", "linkx-debug.ndjson")
    bad = collections_scan(log)
    print("埋点异常:", json.dumps(bad, ensure_ascii=False))
    print("\n=== 结论 ===")
    print("FAILS:", len(fails))
    for f in fails:
        print("  -", f)
    return 1 if fails else 0


def shutil_rmtree(d):
    import shutil

    shutil.rmtree(d, ignore_errors=True)


def collections_scan(log):
    import collections

    c = collections.Counter()
    try:
        for line in open(log, encoding="utf-8", errors="replace"):
            if not line.startswith("{"):
                continue
            try:
                e = json.loads(line).get("event", "")
            except json.JSONDecodeError:
                continue
            if e in (
                "orphan_chunk",
                "file.late_frame",
                "tcp.queue_full",
                "tcp.rx.drop_stale",
                "tcp.prebind_drop",
                "log_dropped",
            ):
                c[e] += 1
    except OSError as ex:
        return {"scan_failed": str(ex)}
    return dict(c)


if __name__ == "__main__":
    sys.exit(main())
