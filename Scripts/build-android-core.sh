#!/usr/bin/env bash
# 编译 Rust Core 为 arm64-v8a .so → Android jniLibs 目录
# 产物：Target/android-jni/arm64-v8a/liblinkx_core.so（含 JNI 导出 feature）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ANDROID_HOME=${ANDROID_HOME:-$ROOT/Tools/Toolchain/Android-SDK}
ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk}
export ANDROID_HOME ANDROID_NDK_HOME
[ -d "$ANDROID_NDK_HOME" ] || { echo "❌ NDK 未找到（$ANDROID_NDK_HOME）"; exit 1; }

rustup target add --toolchain 1.97.1 aarch64-linux-android >/dev/null 2>&1 || true

# 调试变体：DEBUGD=1 时额外启用 agent-debug（本机回环控制面）。
# **交付路径不得设置该变量** —— 与 Windows 侧同一口径：不启用即完全不参与编译，
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

# 校验 JNI 导出符号。readelf 取 NDK 自带并按宿主补后缀（Windows 上是 .exe 且不在 PATH）。
# 旧写法 `llvm-readelf … | grep … || true` 在 Windows 宿主上静默空跑，
# 让"校验"看起来通过而实际什么都没查 —— 故缺工具/零符号一律硬失败，不再吞错。
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