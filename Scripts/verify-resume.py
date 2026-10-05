#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""断点续传「迟到的续传」真机验收。

跑之前要有：

- 电脑端跑调试版：`cargo build --target x86_64-pc-windows-gnu -p linkx-windows --features agent-debug`
- 手机端装调试包（`bash Scripts/build-android-apk.sh`，要带 `DEBUGD=1` 才有控制面），
  且两端**已配对**
- `adb forward tcp:55700 tcp:55699`（手机侧控制面经 USB 透出）

四段各判一件事：

1. 手机发端掉一帧 → 电脑暂缓收尾 → 手机补发。必须是「已完成」且字节数与声明一致；
   过程证据是电脑埋点里 `recv.hold_for_resume` 在、`recv.hold_expired` 与 `orphan_chunk` 不在。
2. 电脑发端掉一帧 → 手机暂缓收尾 → 电脑在**回执窗口里**重启补发。两端都要「已完成」，
   且 `send.resume_in_ack_window` 必须在——这条分支只有新代码会走。
3-4. 不注错的对照各一轮：补发改造不许把正常传输弄坏（此时上述事件都该是 0）。

只认"两端都说完话"的终态。「等待对端确认」「重传中」「续传中」都不是结论。

用法：
  python Scripts/verify-resume.py [all|1,2,3,4] [--video-id N] [--video-size 字节]
                                  [--device-name 子串]

手机相册里的视频与目标设备默认**自动发现**（见 `--help`），本机那台手机的 id 与尺寸不写死。
"""
import argparse
import json
import math
import os
import pathlib
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request

# 仓库根由脚本自身位置推导，中间产物不写死任何人的盘符
ROOT = pathlib.Path(__file__).resolve().parents[1]
PC = "http://127.0.0.1:55699"
PHONE = "http://127.0.0.1:55700"
CHUNK = 262144
OUT = str(ROOT / "Temp" / "resume")
# 这些都是"还没说完话"的中间态（「等待对端确认」尤其：几百毫秒后就会变已完成）
NON_TERMINAL = ("接收中", "发送中", "重传中", "传输中", "等待对端确认", "续传中", "已发完（结束帧未发出）")
# 手机→电脑两段用的视频（id + 声明字节），main() 里解析后填入；不写死某台手机上的条目
VIDEO = {}
ARGS = None


def get(base):
    return json.loads(urllib.request.urlopen(base + "/state", timeout=10).read().decode("utf-8"))


def post(base, name, **kw):
    q = urllib.parse.urlencode(kw)
    req = urllib.request.Request(f"{base}/action/{name}?{q}", method="POST")
    return json.loads(urllib.request.urlopen(req, timeout=25).read().decode("utf-8"))


def pc_tasks():
    return get(PC).get("ui_file_tasks") or []


def phone_rows_text():
    """手机端 `file_rows` 是一个字符串（`名字#fileId=状态` 逗号分隔），不是结构化列表。"""
    return str((get(PHONE).get("host") or {}).get("file_rows") or "")


def ndjson_path():
    return os.path.join(os.environ["APPDATA"], "LinkX", "Logs", "linkx-debug.ndjson")


def events(names, since_ms):
    """电脑侧埋点里挑出指定事件：`{事件名: [(ts, fields)]}`。

    判"这条分支真的走过"只认它——界面状态几百毫秒就翻篇，靠轮询采样一定会漏。
    """
    out = {n: [] for n in names}
    with open(ndjson_path(), encoding="utf-8", errors="replace") as f:
        for line in f:
            if not line.startswith("{"):
                continue
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            n = e.get("event")
            if n in out and e.get("ts_ms", 0) >= since_ms:
                out[n].append((e["ts_ms"], e.get("fields") or {}))
    return out


def wait(predicate, timeout, interval=1.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        v = predicate()
        if v:
            return v, time.time() - t0
        time.sleep(interval)
    return None, time.time() - t0


def wait_started(pick, timeout=90):
    """先等到这条行真的在途。同名行可能是上一轮的终态残留，直接等"终态"会把旧话当结论。"""
    return wait(lambda: (lambda r: r if r and r.get("state") in NON_TERMINAL else None)(pick()), timeout)


def wait_terminal(pick, timeout=300):
    return wait(
        lambda: (lambda r: r if r and r.get("state") not in NON_TERMINAL else None)(pick()),
        timeout,
    )


def start_logcat():
    """后台抓手机日志：手机上没有可读的历史缓冲区，logcat 是唯一通道，而它被信标日志冲得很快。"""
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, "logcat.txt")
    fh = open(path, "w", encoding="utf-8", errors="replace")
    proc = subprocess.Popen(["adb", "logcat", "-v", "time"], stdout=fh, stderr=subprocess.DEVNULL)
    return proc, fh, path


def stop_logcat(proc, fh, path, keys):
    proc.terminate()
    fh.close()
    return [
        ln.strip()
        for ln in open(path, encoding="utf-8", errors="replace")
        if "LinkX" in ln and any(k in ln for k in keys)
    ]


def make_file(size):
    os.makedirs(OUT, exist_ok=True)
    p = os.path.join(OUT, f"pc-{size}-{int(time.time())}.bin")
    with open(p, "wb") as fh:
        block = bytes(((i * 7 + 13) & 0xFF for i in range(65536)))
        written = 0
        while written < size:
            n = min(len(block), size - written)
            fh.write(block[:n])
            written += n
    return p


def ensure_link():
    s = get(PC)
    if s.get("paired") and s.get("engine_tcp_bound"):
        return True
    devs = s.get("devices") or []
    if ARGS and ARGS.device_name:
        devs = [d for d in devs if ARGS.device_name in str(d.get("name"))]
    if not devs:
        print("❌ 设备表里没有手机"
              + (f"（名字含 {ARGS.device_name!r} 的设备为空）" if ARGS and ARGS.device_name else "")
              + "（手机端没在广播？先 `am start` 拉起来）")
        return False
    # 列表按发现顺序追加，最后一条最可能是当前这台手机；同时挂着多台时用 --device-name 收窄
    post(PC, "connect", addr=devs[-1]["addr"])
    ok, _ = wait(
        lambda: (lambda q: (q.get("paired") and q.get("engine_tcp_bound")) or None)(get(PC)),
        timeout=240,
    )
    if not ok:
        print("❌ 链路起不来，最近 errors:", json.dumps(get(PC).get("errors") or [], ensure_ascii=False)[-260:])
    return bool(ok)


def resolve_video(want_id, want_size):
    """确定手机→电脑两段用的视频（id、声明字节、扩展名）；返回 (id, 字节, 后缀, 失败原因)。

    自动发现走 `/action/album-list?page=0` → 轮询 `/state.ui_album_items`：
    `page` 从 **0** 起算（从 1 起步会拿到空清单），而清单是异步回填的，下发后必须轮询。
    默认取清单里体积最大的视频——"在最后一帧附近掉一帧"要落在有意义的分块序号上。
    声明字节必须拿到实值：判据是"落盘字节与声明一致"，把尺寸写死等于把判据绑在某一台手机上。
    """
    items = []
    for _ in range(40):
        post(PC, "album-list", page=0, per=48)
        time.sleep(1)
        items = get(PC).get("ui_album_items") or []
        if items:
            break
    if not items:
        return None, None, None, "相册清单 40 秒内是空的（手机侧相册页没加载出来）：可显式给 --video-id 与 --video-size"
    vid = int(want_id) if want_id else None
    rec = None
    if vid is None:
        vids = [it for it in items if it.get("kind") == "video"]
        if not vids:
            return None, None, None, f"清单里 {len(items)} 条没有一条是视频：请显式给 --video-id"
        rec = max(vids, key=lambda it: int(it.get("size") or 0))
        vid = int(rec["id"])
    else:
        rec = next((it for it in items if int(it.get("id") or -1) == vid), None)
    size = int(want_size) if want_size else int((rec or {}).get("size") or 0)
    if not size:
        return None, None, None, (f"id={vid} 的声明字节拿不到（清单里没有这条，或 size 为 0）："
                                  "请同时给 --video-size <字节>，否则没法安排「最后一帧附近掉帧」")
    suffix = os.path.splitext(str((rec or {}).get("name") or ""))[1].lower() or ".mp4"
    return vid, size, suffix, None


def case_phone_to_pc(drop):
    tag = "1 手机发端掉帧 → 电脑暂缓收尾 → 手机补发" if drop else "3 对照：手机 → 电脑（不注错）"
    print(f"\n=== {tag} ===")
    if not VIDEO:
        vid, size, suffix, why = resolve_video(ARGS.video_id, ARGS.video_size)
        if not vid:
            print(f"  ❌ {why}")
            return False
        VIDEO.update(id=vid, size=size, suffix=suffix)
        print(f"  样本视频：id={vid} 声明 {size} B 后缀 {suffix}（自动发现或按参数指定）")
    video_id, video_size = VIDEO["id"], VIDEO["size"]
    total = math.ceil(video_size / CHUNK)
    d = os.path.join(OUT, "p2p")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    t0 = int(time.time() * 1000)
    cap = start_logcat()
    if drop:
        print("  手机侧:", post(PHONE, "drop-chunk", at=total - 2).get("result"))
    print("  电脑侧:", post(PC, "album-export", id=video_id, dir=d.replace("\\", "/")).get("result"))
    # 接收任务按「方向 + 这条视频的后缀」认；后缀跟着自动发现的条目走，不写死 .mp4
    pick = lambda: next((t for t in pc_tasks()
                         if t["dir"] == "recv" and t["name"].lower().endswith(VIDEO["suffix"])), None)
    started, _ = wait_started(pick)
    if not started:
        print("  ❌ 没看到这条接收任务起播（导出根本没开始？）")
        stop_logcat(*cap, ["LinkX"])
        return False
    row, _ = wait_terminal(pick)
    got = sum(os.path.getsize(os.path.join(d, f)) for f in os.listdir(d))
    ev = events({"recv.hold_for_resume", "recv.hold_expired", "orphan_chunk"}, t0)
    lines = stop_logcat(*cap, ["补发", "接收完成", "回执", "暂缓"])
    print("  电脑任务行:", json.dumps(row, ensure_ascii=False))
    print(f"  落盘 {got} B / 声明 {video_size} B")
    print("  埋点计数:", {k: len(v) for k, v in ev.items()})
    print("  手机侧日志:", json.dumps(lines[-3:], ensure_ascii=False))
    if drop:
        ok = (
            bool(row)
            and row["state"] == "已完成"
            and got == video_size
            and bool(ev["recv.hold_for_resume"])
            and not ev["recv.hold_expired"]
            and not ev["orphan_chunk"]
        )
    else:
        ok = bool(row) and row["state"] == "已完成" and got == video_size
    print(f"  {'✅' if ok else '❌'} {tag}")
    return ok


def case_pc_to_phone(drop):
    tag = "2 电脑发端掉帧 → 手机暂缓收尾 → 电脑在回执窗口里补发" if drop else "4 对照：电脑 → 手机（不注错）"
    print(f"\n=== {tag} ===")
    size = 50 * 1024 * 1024
    total = math.ceil(size / CHUNK)
    p = make_file(size)
    name = os.path.basename(p)
    t0 = int(time.time() * 1000)
    cap = start_logcat()
    if drop:
        print("  电脑侧:", post(PC, "drop-chunk", at=total - 2).get("result"))
    print("  电脑侧:", post(PC, "send-file", path=p.replace("\\", "/")).get("result"))
    pick = lambda: next((t for t in pc_tasks() if t["name"] == name), None)
    started, _ = wait_started(pick)
    if not started:
        print("  ❌ 没看到这条发送任务起播")
        stop_logcat(*cap, ["LinkX"])
        return False
    row, _ = wait_terminal(pick)
    ev = events({"send.resume_in_ack_window", "send.resume_capped", "resume.adopt", "recv.done_no_session"}, t0)
    lines = stop_logcat(*cap, ["补发", "接收完成", "暂缓", "回执"])
    print("  电脑发端任务行:", json.dumps(row, ensure_ascii=False))
    print("  手机端这一条:", next((seg for seg in phone_rows_text().split(",") if name in seg), "（没读到）"))
    print("  埋点计数:", {k: len(v) for k, v in ev.items()})
    print("  手机侧日志:", json.dumps(lines[-3:], ensure_ascii=False))
    ok = bool(row) and row["state"] == "已完成"
    if drop:
        # 只有新代码会走这条分支；没有它说明这次压根没发生"迟到的续传"，这一段等于没验
        ok = ok and bool(ev["send.resume_in_ack_window"]) and not ev["send.resume_capped"]
    print(f"  {'✅' if ok else '❌'} {tag}")
    return ok


def main():
    global ARGS
    ap = argparse.ArgumentParser(description="断点续传「迟到的续传」真机验收")
    ap.add_argument("cases", nargs="?", default="all",
                    help="跑哪几段：all 或逗号分隔的 1/2/3/4（默认 all）")
    ap.add_argument("--video-id", type=int, default=0,
                    help="手机相册里用于「手机→电脑」的条目 id；默认 0 = 自动取清单里体积最大的视频")
    ap.add_argument("--video-size", type=int, default=0,
                    help="该条目的声明字节数；默认 0 = 从相册清单的 size 字段读，不写死某台手机的尺寸")
    ap.add_argument("--device-name", default="",
                    help="按名字子串过滤设备表；默认留空 = 自动取设备表最后一条（最近发现的设备）")
    ARGS = ap.parse_args()
    which = ARGS.cases
    todo = {
        "1": lambda: case_phone_to_pc(True),
        "2": lambda: case_pc_to_phone(True),
        "3": lambda: case_phone_to_pc(False),
        "4": lambda: case_pc_to_phone(False),
    }
    if not ensure_link():
        return 1
    print("链路就绪")
    rc = 0
    for k in (sorted(todo) if which == "all" else which.split(",")):
        rc |= 0 if todo[k]() else 1
    print("\n=== 结论 ===")
    print("PASS" if rc == 0 else "FAIL")
    return rc


if __name__ == "__main__":
    sys.exit(main())
