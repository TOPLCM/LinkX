#!/usr/bin/env bash
# 交付构建"零调试残留"校验。
#
# `agent-debug` 是 optional feature，理论上交付构建里根本不该编进控制面 —— 但"理论上"
# 不等于"验证过"，本项目反复吃过"配置说关了、产物里却还在"的亏，所以直接对产物字节断言。
# 反向断言（产品该有的符号必须在）同样不可省：扫不到残留也可能是**扫错了文件**或
# .so 是旧的，那两种情况都会假装成"干净"。
#
# 用法：
#   bash Scripts/check-release-clean.sh                      # 默认查 Windows release exe
#   bash Scripts/check-release-clean.sh path/to/linkx.exe    # 查指定 exe
#   bash Scripts/check-release-clean.sh --apk-so path/to.apk # 解出 APK 里的 .so 与 dex 再查
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

STRINGS="$(command -v strings || echo '')"
[ -n "$STRINGS" ] || echo "  （未找到 strings，改用 grep -a 直接扫字节）"

# 命中即失败：这些串只可能来自 linkx-debugd / agent-debug 分支
# 注：端口 55699 是 u16 立即数、`/state`+`/action/*` 是运行期拼接，
# 不会以连续字面量出现在产物里 —— 它们挡不住控制面，真正的闸门是下面的 feature_gate。
BAD_PATS=(linkx-debugd nativeDebugdStart nativeDebugTakeRequest nativeDebugSetField
  nativeDebugCounter nativeDebugAddRequestLimit serde_json 'LocalSend' 'localsend')
# classes.dex 专用：Kotlin 侧的 `external fun nativeDebug*` 是 JNI 入口，proguard 必须 keep，
# R8 删不掉 —— 它们在 dex 里出现**不代表**调试面能起来（实现在 .so 里，上面那份扫描已证明没有）。
# 所以这一层只查"真正会跑起来的东西"。
BAD_PATS_DEX=(linkx-debugd serde_json 'LocalSend' 'localsend')

fail=0

scan() { # scan <文件> <反向断言串> <说明> [模式数组名，默认 BAD_PATS]
  local f="$1" must="$2" label="$3" patvar="${4:-BAD_PATS[@]}"
  echo "== 交付构建零调试残留：$label =="
  [ -f "$f" ] || { echo "❌ 找不到产物 $f"; fail=1; return; }

  # 先把可打印串一次性落到临时文件再 grep。原来的 `strings "$f" | grep -q` 在
  # pipefail 下不可靠：grep -q 命中即关管道 → strings 收到 SIGPIPE(141) →
  # 整条管道判失败，**明明有残留也会被读成"没有"**（假绿）。
  local dump
  dump="$(mktemp)"
  if [ -n "$STRINGS" ]; then
    "$STRINGS" -a "$f" > "$dump" 2>/dev/null || true
  else
    tr -cd ' -~' < "$f" > "$dump"
  fi
  has() { grep -q "$1" "$dump"; }

  local bad=0
  for pat in "${!patvar}"; do
    if has "$pat"; then
      echo "  ✗ 命中调试残留：$pat"
      bad=1
    fi
  done
  [ "$bad" = 0 ] && echo "  ✓ 无控制面符号 / 路由串 / serde_json / LocalSend 字样"

  # 反向断言：产品该有的东西必须在（扫错文件、或产物是旧的，都在这一步露出来）
  if has "$must"; then
    echo "  ✓ 产品符号在位（$must）"
  else
    echo "  ✗ 找不到产品符号 $must —— 要么扫错了文件，要么这份产物是旧的"
    bad=1
  fi
  rm -f "$dump"
  [ "$bad" = 0 ] || fail=1
}

# 端口号是 u16 立即数、路由串是运行期拼接，两者都**不会**以连续字面量出现在产物里，
# 所以"字节扫描"这一层挡不住控制面被编进去。真正的判据是依赖图：
# linkx-debugd 这个 crate 没进图，产物里就不可能有它。
feature_gate() {
  echo "== 依赖图闸门：交付构建不得带 agent-debug =="
  local tree
  if ! tree="$(cargo tree -e features -p linkx-windows --target x86_64-pc-windows-gnu 2>&1)"; then
    echo "  ✗ cargo tree 跑失败：$(echo "$tree" | tail -2)"
    fail=1
    return
  fi
  if echo "$tree" | grep -qE 'linkx-debugd|serde_json v'; then
    echo "  ✗ 交付构建的依赖图里出现了调试面 crate："
    echo "$tree" | grep -E 'linkx-debugd|serde_json v' | sed 's/^/      /'
    fail=1
  else
    echo "  ✓ 依赖图里没有 linkx-debugd / serde_json"
  fi
}

if [ "${1:-}" = "--apk-so" ]; then
  APK="${2:-}"
  [ -n "$APK" ] || { echo "用法：$0 --apk-so <apk 路径>"; exit 1; }
  TMP="Temp/release-clean-liblinkx_core.so"
  mkdir -p Temp
  # APK 里每个 ABI 一份 .so；交付只发 arm64-v8a，就取那一份
  if ! unzip -p "$APK" "lib/arm64-v8a/liblinkx_core.so" > "$TMP" 2>/dev/null; then
    echo "❌ $APK 里没有 lib/arm64-v8a/liblinkx_core.so"
    exit 1
  fi
  scan "$TMP" 'Java_com_linkx_app_NativeCore_nativeSendMediaState' "APK 内 liblinkx_core.so（$APK）"
  rm -f "$TMP"
  # .so 干净不代表 APK 干净：Kotlin 侧的 `nativeDebugdStart` 声明、端口常量、动作名
  # 都在 classes.dex 里。交付包把这些带着，等于对外声称"这是调试构建"。
  # classes.dex 这一层能验的是"调试面在交付包里到底跑不跑得起来"。
  # Kotlin 侧那四个 `external fun nativeDebug*` 声明**必然**留在 dex 里：
  # 它们是 JNI 入口，proguard 规则要求 keep，R8 删不掉。真正决定成败的是上面那份 .so
  # —— 里面没有对应符号，声明就是死引用（调用点已被 BuildConfig.DEBUG 折掉，
  # 且 runCatching 兜住 UnsatisfiedLinkError）。所以这里断言"跑不起来"，
  # 而不是要求 dex 里连名字都不许出现。
  DEX="Temp/release-clean-classes.dex"
  if unzip -p "$APK" "classes.dex" > "$DEX" 2>/dev/null; then
    scan "$DEX" 'nativeSendMediaState' "APK 内 classes.dex（$APK）" BAD_PATS_DEX[@]
    if grep -aq 'debugdAvailable = true' "$DEX"; then
      echo "  ✗ classes.dex 里有把调试面判为可用的赋值"
      fail=1
    fi
  else
    echo "  ✗ $APK 里取不到 classes.dex，无法核验 APK 层残留"
    fail=1
  fi
  rm -f "$DEX"
else
  EXES=("$@")
  [ ${#EXES[@]} -gt 0 ] || EXES=("Target/x86_64-pc-windows-gnu/release/linkx.exe")
  for e in "${EXES[@]}"; do
    scan "$e" 'LinkX Core' "$e"
  done
  feature_gate
fi

[ "$fail" = 0 ] || { echo "交付构建卫生检查未通过"; exit 1; }
echo "✅ 交付构建干净"
