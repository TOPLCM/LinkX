#!/usr/bin/env bash
# 交付构建"零调试残留"校验：直接对产物字节断言，不采信配置声明。
#
# `agent-debug` 是 optional feature，交付构建里不该编进控制面；但"没开"不等于"产物里
# 真的没有"，所以两层都要查：正向查残留符号，反向查产品该有的符号必须在 ——
# 只查前者时，扫错文件或产物是旧的都会假装成"干净"。
#
# 用法：
#   bash Scripts/check-release-clean.sh                      # 默认查 Windows release exe
#   bash Scripts/check-release-clean.sh path/to/linkx.exe    # 查指定 exe
#   bash Scripts/check-release-clean.sh --apk-so path/to.apk # 解出 APK 里的 .so 与 dex 再查
#   LINKX_MUST_SYMBOL=… LINKX_MUST_SYMBOL_DEX=… 同上          # 复核历史产物时换成那一版该有的符号
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

STRINGS="$(command -v strings || echo '')"
[ -n "$STRINGS" ] || echo "  （未找到 strings，改用 grep -a 直接扫字节）"

# 命中即失败：这些串只可能来自 linkx-debugd / agent-debug 分支。
# 端口与路由串不会以连续字面量出现，字节层的局限见 feature_gate 上方。
BAD_PATS=(linkx-debugd nativeDebugdStart nativeDebugTakeRequest nativeDebugSetField
  nativeDebugCounter nativeDebugAddRequestLimit serde_json 'LocalSend' 'localsend')
# classes.dex 专用：Kotlin 侧的 `external fun nativeDebug*` 是 JNI 入口，proguard 必须 keep，
# R8 删不掉；它们出现在 dex 里不代表调试面能起来。这一层只查真正会跑起来的东西。
BAD_PATS_DEX=(linkx-debugd serde_json)
# 许可署名例外：Apache-2.0 §4(b)/(d) 要求随分发附带的上游署名与改动声明里写出上游项目名，
# 所以「关于」页的清单条目与那三个许可文件名里必然出现 LocalSend / localsend。
# 判据不是"出现即失败"，而是"**只允许出现这些字面量**"——期望次数从下面这份清单推导，
# 实际次数从产物里数，两者必须相等：多一次就是别处混进了 LocalSend（这道门禁要防的正是它），
# 少一次说明这份 APK 没带上应有的署名。清单外的任何写法一律红。
ALLOWED_ATTRIBUTION=(
  'LICENSE-localsend.txt'
  'NOTICE-localsend.txt'
  'PORTING-localsend.md'
  'LocalSend 移植件许可（Apache-2.0）'
  'LocalSend 上游署名'
  'LocalSend 移植范围与改动声明'
)

fail=0

scan() { # scan <文件> <反向断言串> <说明> [模式…，默认 BAD_PATS]
  local f="$1" must="$2" label="$3"
  shift 3 2>/dev/null || true
  local -a pats=("$@")
  [ ${#pats[@]} -gt 0 ] || pats=("${BAD_PATS[@]}")
  echo "== 交付构建零调试残留：$label =="
  [ -f "$f" ] || { echo "❌ 找不到产物 $f"; fail=1; return; }

  # 可打印串先一次性落到临时文件再 grep：`strings "$f" | grep -q` 在 pipefail 下，
  # grep -q 命中即关管道 → strings 收到 SIGPIPE(141) → 整条管道判失败，
  # 于是有残留也会被读成"没有"。
  local dump
  dump="$(mktemp)"
  if [ -n "$STRINGS" ]; then
    "$STRINGS" -a "$f" > "$dump" 2>/dev/null || true
  else
    tr -cd ' -~' < "$f" > "$dump"
  fi
  has() { grep -q "$1" "$dump"; }

  local bad=0
  local pat
  for pat in "${pats[@]}"; do
    if has "$pat"; then
      echo "  ✗ 命中调试残留：$pat"
      bad=1
    fi
  done
  [ "$bad" = 0 ] && echo "  ✓ 未命中禁用模式（${pats[*]}）"

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
  # 查的目标必须与这份产物一致：给 arm64 包查 gnu 目标的依赖图等于没查。
  # 目标由出包脚本传进来（LINKX_GATE_TARGET），单独跑时默认还是交付主目标 x64/gnu。
  local tree target="${LINKX_GATE_TARGET:-x86_64-pc-windows-gnu}"
  if ! tree="$(cargo tree -e features -p linkx-windows --target "$target" 2>&1)"; then
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

# 计数：产物里某字面量出现的次数。跑在原始字节上，不跑在"可打印串"视图上 ——
# 后者会把非打印字节删掉再拼接，于是两个本不相邻的串可能被凑成一次假命中。
count_occ() { # count_occ <字面量> <文件>
  { LC_ALL=C grep -ao -- "$1" "$2" 2>/dev/null || true; } | wc -l | tr -d ' '
}

# 署名例外判据：只统计 APK 的**代码层**（dex + 资源表），
# 许可正文本身（assets 与安装目录里的那几份文件）当然写着上游项目名，那是义务、不是残留。
attribution_gate() { # attribution_gate <APK>
  local apk="$1" dir="Temp/release-clean-attribution"
  rm -rf "$dir"; mkdir -p "$dir"
  unzip -o -q "$apk" 'classes*.dex' 'resources.arsc' -d "$dir" 2>/dev/null || true
  local files
  files="$(find "$dir" -type f)"
  if [ -z "$files" ]; then
    echo "  ✗ APK 里取不到 dex / 资源表，无法核验许可署名"
    fail=1; return
  fi
  local want_ls want_lsend got_ls got_lsend
  want_ls="$(printf '%s\n' "${ALLOWED_ATTRIBUTION[@]}" | { LC_ALL=C grep -ao 'LocalSend' || true; } | wc -l | tr -d ' ')"
  want_lsend="$(printf '%s\n' "${ALLOWED_ATTRIBUTION[@]}" | { LC_ALL=C grep -ao 'localsend' || true; } | wc -l | tr -d ' ')"
  got_ls=0; got_lsend=0
  local f
  for f in $files; do
    got_ls=$((got_ls + $(count_occ 'LocalSend' "$f")))
    got_lsend=$((got_lsend + $(count_occ 'localsend' "$f")))
  done
  rm -rf "$dir"
  if [ "$got_ls" != "$want_ls" ] || [ "$got_lsend" != "$want_lsend" ]; then
    echo "  ✗ 代码层里的 LocalSend/localsend 次数对不上许可署名清单"
    echo "     LocalSend  期望 $want_ls 次，实际 $got_ls 次"
    echo "     localsend  期望 $want_lsend 次，实际 $got_lsend 次"
    echo "     多出来的是混进来的上游实现，少掉的是该随包走的署名"
    fail=1
  else
    echo "  ✓ 代码层只有许可署名清单里的 LocalSend/localsend（$got_ls / $got_lsend 次）"
  fi
}

# 反向断言用的"这一版该有的符号"。默认按当前交付构建；**复核历史归档产物时要能换**：
# 媒体指令是 0.4.0 才有的，拿它去断言 0.1.0~0.3.0 的 APK 只会得到一条假的红。
MUST_SO="${LINKX_MUST_SYMBOL:-Java_com_linkx_app_NativeCore_nativeSendMediaState}"
MUST_DEX="${LINKX_MUST_SYMBOL_DEX:-nativeSendMediaState}"

if [ "${1:-}" = "--apk-so" ]; then
  APK="${2:-}"
  [ -n "$APK" ] || { echo "用法：$0 --apk-so <apk 路径>"; exit 1; }
  mkdir -p Temp
  # 0.5.3 起一个 APK 带两套 ABI，每套单独扫一遍：32 位那份是另一个编译器、另一条链接命令行产出的，
  # arm64 干净不代表 v7a 干净。复核单 ABI 的历史归档时把 LINKX_EXPECT_ABIS 换成 "arm64-v8a"。
  ENTRIES="$(unzip -Z1 "$APK" 'lib/*/liblinkx_core.so' 2>/dev/null || true)"
  [ -n "$ENTRIES" ] || { echo "❌ $APK 里取不到任何 lib/<abi>/liblinkx_core.so"; exit 1; }
  FOUND_ABIS=""
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    abi="$(basename "$(dirname "$entry")")"
    FOUND_ABIS="$FOUND_ABIS $abi"
    TMP="Temp/release-clean-liblinkx_core-$abi.so"
    unzip -p "$APK" "$entry" > "$TMP" 2>/dev/null || { echo "  ✗ 解出 $entry 失败"; fail=1; continue; }
    scan "$TMP" "$MUST_SO" "APK 内 $entry（$APK）"
    rm -f "$TMP"
  done <<< "$ENTRIES"
  for want in ${LINKX_EXPECT_ABIS:-arm64-v8a armeabi-v7a}; do
    case " $FOUND_ABIS " in *" $want "*) ;; *) echo "  ✗ APK 里没有 $want 那套 .so"; fail=1 ;; esac
  done
  # .so 干净不代表 APK 干净：Kotlin 侧的 `external fun nativeDebug*` 声明还在 classes.dex 里。
  # 它们是 JNI 入口、proguard 必须 keep、R8 删不掉，所以这一层验的是"跑不跑得起来"：
  # .so 里没有对应符号时声明就是死引用（调用点已被 BuildConfig.DEBUG 折掉，
  # runCatching 兜住 UnsatisfiedLinkError），不要求 dex 连名字都不许出现。
  DEX="Temp/release-clean-classes.dex"
  if unzip -p "$APK" "classes.dex" > "$DEX" 2>/dev/null; then
    scan "$DEX" "$MUST_DEX" "APK 内 classes.dex（$APK）" "${BAD_PATS_DEX[@]}"
    if grep -aq 'debugdAvailable = true' "$DEX"; then
      echo "  ✗ classes.dex 里有把调试面判为可用的赋值"
      fail=1
    fi
  else
    echo "  ✗ $APK 里取不到 classes.dex，无法核验 APK 层残留"
    fail=1
  fi
  rm -f "$DEX"
  attribution_gate "$APK"
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
