#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
linkx-ctl —— LinkX 双端调试控制面的第二层薄封装。

控制面本身是两端各自的回环 HTTP/JSON（Windows :55699，安卓经
`adb forward tcp:55700 tcp:55699`）。curl 已能直接驱动，本脚本只解决三件
curl 做起来会出错的事：

1. **UTF-8 参数**：curl 把裸 UTF-8 塞进 URL，控制面按原始字节解 query，中文剪贴板
   内容会变成乱码进产品路径。这里统一 `quote()`。
2. **双端对照**：判断"两端是否真的一致"必须同时读两份 JSON 再比字段，shell 里
   比对容易看错（本项目出过"拿不同握手轮次的 SAS 互比"的误判）。
3. **轮询等状态**：真机验收要的是"多久进入 Paired / 有没有 -212"，不是截一张图。

只用标准库：这台机器上不必为调试脚本引入任何依赖，也不扩大供应链审计面。

用法示例：
  python Scripts/linkx-ctl.py status
  python Scripts/linkx-ctl.py pair
  python Scripts/linkx-ctl.py action win send-clip text="你好 αβγ"
  python Scripts/linkx-ctl.py clip-roundtrip
  python Scripts/linkx-ctl.py logs and 40
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ENDS = {"win": "http://127.0.0.1:55699", "and": "http://127.0.0.1:55700"}
LABEL = {"win": "Windows", "and": "Android"}


def http(end: str, path: str, post: bool = False, timeout: float = 5.0):
    """GET（默认）或 POST 到某一端的控制面；查询串必须写在 path 里。

    `POST /action/<name>?k=v` 的语义是**从请求行的 target 取 query**（见
    `Crates/debugd` 的路由实现），所以参数一定要拼在 URL 上、而不是塞进表单体——
    用 body 发过去，控制面会收到一个没有参数的动作并回"缺参数"。
    """
    url = ENDS[end] + path
    req = urllib.request.Request(
        url, data=b"" if post else None, method="POST" if post else "GET"
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read().decode("utf-8", "replace")
    if path.startswith("/logs"):
        return [json.loads(l) for l in raw.splitlines() if l.strip()]
    return json.loads(raw)


def state(end: str) -> dict:
    return http(end, "/state")


# ---------- 视图 ----------


def kv(end: str) -> dict:
    """把两端结构不同的 /state 压平成同一组键，这样才可能横向比对。"""
    s = state(end)
    if end == "and":
        e = (s.get("engines") or [{}])[0]
        h = s.get("host") or {}
        return {
            "phase": e.get("state"),
            "paired": e.get("paired"),
            "sas": e.get("sas"),
            "own_fp": e.get("own_fp"),
            "peer_fp": e.get("peer_fp"),
            "mtu": e.get("ble_mtu"),
            "out_pending": e.get("out_pending"),
            "tcp_bound": h.get("tcp_bound"),
            "clip": h.get("clip"),
            "clip_sync": h.get("clip_sync"),
            "theme": h.get("theme"),
            "peer_ip": h.get("peer_ip"),
            "lan_err": h.get("lan_err"),
            "lan_attempts": h.get("lan_attempts"),
            "last_action": h.get("last_action"),
        }
    return {
        "phase": s.get("phase"),
        "paired": s.get("paired"),
        "sas": s.get("sas"),
        "own_fp": s.get("own_fp"),
        "peer_fp": None,
        "mtu": s.get("ble_mtu"),
        "out_pending": s.get("ble_out_pending"),
        # Windows 有两个"绑定"：app 级只表示发起过，engine 级才是 4.6.4 走完。
        # 文件发送卡在"等待 TCP 通道"时，只看前者会误判成已就绪。
        "tcp_bound": "yes" if s.get("engine_tcp_bound") else "no",
        "tcp_bind_started": s.get("tcp_bind_started"),
        "ble_connected": s.get("ble_connected"),
        "clip": s.get("clip"),
        "clip_sync": None,
        "theme": None,
        "peer_ip": None,
        "last_action": None,
        "errors": s.get("errors"),
        "devices": s.get("devices"),
    }


def cmd_status(_a):
    for end in ("win", "and"):
        print(f"[{LABEL[end]}]")
        for k, v in kv(end).items():
            if v is not None:
                print(f"  {k:12} {v}")


def cmd_logs(a):
    rows = http(a.end, "/logs?tail=" + str(a.n))
    for r in rows[-a.n:]:
        f = r.get("fields") or {}
        extra = " ".join(f"{k}={v}" for k, v in f.items())
        print(f'{r["ts_ms"]} {r["level"]:5} {r["module"]}/{r["event"]} {extra}')


def cmd_counters(a):
    for end in ([a.end] if a.end != "both" else ["win", "and"]):
        print(f"[{LABEL[end]}] {json.dumps(http(end, '/counters'), ensure_ascii=False)}")


def cmd_action(a):
    path = "/action/" + urllib.parse.quote(a.name)
    if a.kv:
        path += "?" + urllib.parse.urlencode(dict(p.split("=", 1) for p in a.kv))
    print(json.dumps(http(a.end, path, post=True), ensure_ascii=False))


def cmd_raw(a):
    print(json.dumps(state(a.end), ensure_ascii=False, indent=1))


# ---------- 组合动作 ----------


def cmd_pair(a):
    """读 Windows 设备列表 → 下发 connect → 等双端 Paired。全程不碰 GUI。"""
    dev = kv("win").get("devices") or []
    if not dev:
        print("FAIL Windows 设备列表为空（安卓未在广播？）")
        return 1
    # 取最后一条：BLE 临时地址（RPA）会随重启变化，列表按发现顺序追加，
    # 最后一条才可能是当前这台手机——这是目前唯一可用的启发式。
    addr = dev[-1]["addr"]
    print(f"connect -> {addr} ({dev[-1]['name']})")
    cmd_action(argparse.Namespace(end="win", name="connect", kv=[f"addr={addr}"]))
    return wait_paired(60)


def wait_paired(timeout_s: float) -> int:
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        w, d = kv("win"), kv("and")
        if w["paired"] and d["paired"]:
            print(f"OK 双端 Paired，用时 {time.time()-t0:.2f}s")
            print(f"  Windows: {w['phase']} mtu={w['mtu']} sas={w['sas']} fp={w['own_fp']}")
            print(f"  Android: {d['phase']} mtu={d['mtu']} sas={d['sas']} fp={d['own_fp']}")
            return 0
        time.sleep(0.4)
    print(f"FAIL {timeout_s}s 内未双端 Paired")
    print(f"  Windows {kv('win')}")
    print(f"  Android {kv('and')}")
    return 1


def cmd_wait_paired(a):
    return wait_paired(a.timeout)


def cmd_clip_roundtrip(_a):
    """两个方向各发一条唯一标记，读回**对端剪贴板真值**比对。

    只看 `/action` 返回 ok 不算验证——那只证明命令进了队列。
    """
    rc = 0
    for end, peer in (("win", "and"), ("and", "win")):
        mark = f"linkx-{end}2{peer}-{int(time.time())}-中文αβγ"
        r = http(end, "/action/send-clip?" + urllib.parse.urlencode({"text": mark}), post=True)
        print(f"[{LABEL[end]} → {LABEL[peer]}] 发送 ok={r.get('ok')} {mark}")
        got = None
        t0 = time.time()
        while time.time() - t0 < 12:
            time.sleep(0.5)
            got = kv(peer).get("clip")
            if got == mark:
                break
        if got == mark:
            print(f"  对端剪贴板真值一致 ✅")
        else:
            print(f"  对端剪贴板 ≠ 发送内容 ❌ 读到: {got!r}")
            rc = 1
    # 单向 ✅ 也算过是假绿：包装脚本只认这一行总结。
    print("结果：" + ("PASS" if rc == 0 else "FAIL"))
    return rc


def main():
    p = argparse.ArgumentParser(prog="linkx-ctl", description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("status", help="双端关键状态对照").set_defaults(fn=cmd_status)
    sub.add_parser("pair", help="免 GUI 完成配对并等待双端 Paired").set_defaults(fn=cmd_pair)

    w = sub.add_parser("wait-paired", help="只等待，不发起连接")
    w.add_argument("--timeout", type=float, default=45)
    w.set_defaults(fn=cmd_wait_paired)

    for name, help_ in (("raw", "完整 /state"), ("counters", "计数器")):
        s = sub.add_parser(name, help=help_)
        s.add_argument("end", nargs="?", default="both", choices=["win", "and", "both"])
        s.set_defaults(fn=(cmd_raw if name == "raw" else cmd_counters))

    lg = sub.add_parser("logs", help="NDJSON 日志")
    lg.add_argument("end", choices=["win", "and"])
    lg.add_argument("n", nargs="?", type=int, default=30)
    lg.set_defaults(fn=cmd_logs)

    ac = sub.add_parser("action", help="POST /action/<name> k=v ...")
    ac.add_argument("end", choices=["win", "and"])
    ac.add_argument("name")
    ac.add_argument("kv", nargs="*")
    ac.set_defaults(fn=cmd_action)

    sub.add_parser("clip-roundtrip", help="双向往返验证剪贴板同步").set_defaults(fn=cmd_clip_roundtrip)

    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass
    a = p.parse_args()
    try:
        return a.fn(a) or 0
    except urllib.error.URLError as e:
        print(f"控制面不可达：{e}\n（Windows 端需带 agent-debug 启动；安卓需 "
              f"`adb forward tcp:55700 tcp:55699`）")
        return 2


if __name__ == "__main__":
    sys.exit(main())
