#!/usr/bin/env bash
# 编译 Rust Core 为 arm64-v8a .so → Android jniLibs 目录。
# 用法：bash Scripts/build-android-core.sh（可选 DEBUGD=1，见下）
# 产物：Target/android-jni/arm64-v8a/liblinkx_core.so（含 JNI 导出 feature）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ANDROID_HOME / ANDROID_NDK_HOME 优先；未设置时再探测几个常见的 SDK 安装位置。
ANDROID_HOME="${ANDROID_HOME:-}"
if [ -z "$ANDROID_HOME" ]; then
  for c in "$ROOT/Tools/Toolchain/Android-SDK" "$HOME/Android/Sdk" "/opt/android-sdk"; do
    [ -d "$c" ] && ANDROID_HOME="$c" && break
  done
fi
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_HOME:+$ANDROID_HOME/ndk}}"
export ANDROID_HOME ANDROID_NDK_HOME
if [ -z "$ANDROID_NDK_HOME" ] || [ ! -d "$ANDROID_NDK_HOME" ]; then
  echo "❌ 找不到 NDK：请设 ANDROID_HOME 指向 Android SDK，或用 ANDROID_NDK_HOME 直接指到 NDK 目录"
  exit 1
fi

# 目标平台跟 rust-toolchain.toml 走，不在脚本里写死版本
rustup target add aarch64-linux-android >/dev/null 2>&1 || true

# 调试变体：DEBUGD=1 时额外启用 agent-debug（回环控制面）。
# 交付路径不得设置该变量——与 Windows 侧同一口径：不启用即完全不参与编译，
# 交付 .so 里不留符号与端口。
FFI_FEATURES="jni"
if [ "${DEBUGD:-0}" = "1" ]; then
  FFI_FEATURES="jni,agent-debug"
  echo "⚠ DEBUGD=1：产出的是**调试变体** .so（含控制面），不得作为交付物"
fi
echo "  linkx-ffi features: $FFI_FEATURES"
cargo ndk -t arm64-v8a -o Target/android-jni --manifest-path Crates/ffi/Cargo.toml \
  build --release --features "$FFI_FEATURES"

SO="Target/android-jni/arm64-v8a/liblinkx_core.so"
[ -f "$SO" ] || { echo "❌ 未产出 $SO"; exit 1; }
echo "✅ Rust Core .so: $SO"

# 校验 JNI 导出符号：readelf 取 NDK 自带的那个，并按宿主补后缀（Windows 上是 .exe 且不在 PATH）。
# `llvm-readelf … | grep … || true` 在工具缺失时会静默空跑，"校验"于是假装通过 ——
# 故缺工具与零符号都硬失败，不吞错。
READELF=""
for c in "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-readelf \
         "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-readelf.exe; do
  [ -x "$c" ] && { READELF="$c"; break; }
done
[ -n "$READELF" ] || { echo "❌ 未找到 NDK llvm-readelf，无法校验 JNI 导出符号"; exit 1; }
SYMS="$("$READELF" --dyn-syms "$SO" | grep -cE 'Java_com_linkx_app|linkx_ffi|linkx_version' || true)"
[ "${SYMS:-0}" -gt 0 ] \
  || { echo "❌ .so 未导出任何 JNI/FFI 符号（装机后 NativeCore.loadLibrary 必崩）"; exit 1; }
echo "  导出符号命中 $SYMS 个（Java_com_linkx_app* + linkx_*）"