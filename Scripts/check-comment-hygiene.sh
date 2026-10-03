#!/usr/bin/env bash
# 注释卫生门禁（发布前定下的口径，取代"靠人自觉"）
#
# 三条判据，都能机器判定：
#   1) 源码里不出现内部编号 —— BUG-024 / 决策 AO / 续 65 / Spec 4.6.3 / #123 / SEC-06 这类引用
#      在公开仓库里是悬空的：读者手上没有那份台账。修复历史属于 commit message 与
#      发布说明（CHANGELOG），不属于代码。
#   2) 单文件注释占比 ≤ 12%（注释解释"为什么"，不复述代码在做什么）。
#   3) 连续注释块 ≤ 12 行（超长的段落说明这件事该写进 Docs/，或者根本不该写）。
#
# 用法：bash Scripts/check-comment-hygiene.sh [路径…]
#      不带参数查全仓；带路径时只查这些文件/目录（分区收敛时各查各的，不必等全仓绿）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 本机只有 `python`（Git Bash 里没有 python3 这个名字），两个都试
PY="$(command -v python3 || command -v python)"
[ -n "$PY" ] || { echo "❌ 找不到 python"; exit 1; }

"$PY" - "$@" << 'PYEOF'
import os, re, sys

# 本机控制台默认 GBK，打印 ✓/✗ 会抛 UnicodeEncodeError —— 门禁崩掉会被读成"没通过"。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SCOPE = sys.argv[1:] or ["."]
# icons_svg.rs 是生成物；本脚本自身要在注释里写出这些编号长什么样，跳过自己才不会被判违规
# （同 export-public.sh 的 SELF_SKIP：扫描器不能把自己的模式串当成泄露源）
SKIP_NAME = ("icons_svg.rs", "check-comment-hygiene.sh")
REF = re.compile(r"BUG-\d|决策\s+[A-Z]{2}\b|续\s*\d{2,3}|Spec\s+\d|#\d{2,}\b"
                 r"|SEC-\d\d|PANIC-\d\d|DEAD-\d\d|WIN-\d\d|LINKX_ERR")
# 行注释、`#!` shebang 之外的 # 注释、块注释起止。
# 注意 `#[...]` 是 Rust 属性、不是注释：早先把它算进注释，render.rs 凭空多了 253 行
# "注释"，逼着人删有价值的说明去凑比例。**下面这个排除集必须含 `[`** —— 只写 `[@!]`
# 时属性照样被算成注释，注释里写的这条修复其实没生效（2026-10-02 外部审计后自查抓到）。
COMMENT = re.compile(r"^\s*(//+|#(?![\[@!])|/\*|\*/)")
SKIP_DIR = {".git", "Target", "Release", "Temp", "Tools", "node_modules", "Web",
            ".zcode", "spark-output", "build", "gradle", ".cargo"}

fails, checked = [], 0
for base in SCOPE:
    paths = []
    if os.path.isfile(base):
        paths = [base]
    else:
        for root, dirs, files in os.walk(base):
            dirs[:] = [d for d in dirs if d not in SKIP_DIR]
            paths += [os.path.join(root, f) for f in files]
    for p in paths:
        f = os.path.basename(p)
        if not f.endswith((".rs", ".kt", ".proto", ".py", ".sh")) or f in SKIP_NAME:
            continue
        try:
            lines = open(p, encoding="utf-8").read().split("\n")
        except (UnicodeDecodeError, OSError):
            continue
        checked += 1
        total = len(lines)
        com = run = longest = 0
        for i, L in enumerate(lines, 1):
            if not L.lstrip().startswith("#!"):
                m = REF.search(L)
                if m:
                    fails.append("  ✗ %s:%d 内部编号 %r：%s"
                                 % (p, i, m.group(0), L.strip()[:64]))
            if COMMENT.match(L):
                com += 1
                run += 1
                longest = max(longest, run)
            else:
                run = 0
        if total >= 200 and com * 100 // total > 12:
            fails.append("  ✗ %s 注释占比 %d%%（%d/%d），超 12%% 上限"
                         % (p, com * 100 // total, com, total))
        if longest > 12:
            fails.append("  ✗ %s 连续 %d 行注释块（上限 12）：移进 Docs/ 或压成两三行"
                         % (p, longest))

print("== 注释卫生门禁（%d 个文件）==" % checked)
if fails:
    print("\n".join(fails[:60]))
    if len(fails) > 60:
        print("  … 共 %d 条" % len(fails))
    sys.exit(1)
print("  ✓ 无内部编号残留，注释占比与块长度都在上限内")
PYEOF
