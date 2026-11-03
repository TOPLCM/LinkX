#!/usr/bin/env bash
# 编译 Rust Core 为 arm64-v8a + armeabi-v7a 两套 .so → Android jniLibs 目录。
# 用法：bash Scripts/build-android-core.sh（可选 DEBUGD=1，见下）
# 产物：Target/android-jni/<abi>/liblinkx_core.so（两套都必须有；含 JNI 导出 feature）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
. "$ROOT/Scripts/lib-local.sh"

# ANDROID_HOME / ANDROID_NDK_HOME 优先；未设置时再探测几个常见的 SDK 安装位置。
ANDROID_HOME="${ANDROID_HOME:-}"
if [ -z "$ANDROID_HOME" ]; then
  for c in ${LINKX_TOOLCHAIN_ROOT:+"$LINKX_TOOLCHAIN_ROOT/Toolchain/Android-SDK"} "$HOME/Android/Sdk" "/opt/android-sdk"; do
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
# 32 位安卓（armv7）与 64 位（arm64）都要 std，缺任一个 cargo-ndk 会在链接阶段才报错。
rustup target add aarch64-linux-android armv7-linux-androideabi >/dev/null 2>&1 || true

# 调试变体：DEBUGD=1 时额外启用 agent-debug（回环控制面）。
# 交付路径不得设置该变量——与 Windows 侧同一口径：不启用即完全不参与编译，
# 交付 .so 里不留符号与端口。
FFI_FEATURES="jni"
if [ "${DEBUGD:-0}" = "1" ]; then
  FFI_FEATURES="jni,agent-debug"
  echo "⚠ DEBUGD=1：产出的是**调试变体** .so（含控制面），不得作为交付物"
fi
echo "  linkx-ffi features: $FFI_FEATURES"
echo "  linkx-ffi abi: arm64-v8a + armeabi-v7a"
cargo ndk -t arm64-v8a -t armeabi-v7a -o Target/android-jni --manifest-path Crates/ffi/Cargo.toml \
  build --release --features "$FFI_FEATURES"

# 两套 ABI 都得产出：少一套时 Gradle 照样能打出一个"看着正常"的 APK，
# 而那套 ABI 的机器装上就是启动即崩（NativeCore.loadLibrary 失败），所以在这里就拦住。
ABIS=(arm64-v8a armeabi-v7a)
SOS=()
for abi in "${ABIS[@]}"; do
  SO="Target/android-jni/$abi/liblinkx_core.so"
  [ -f "$SO" ] || { echo "❌ 未产出 $SO"; exit 1; }
  SOS+=("$SO")
  echo "✅ Rust Core .so: $SO"
done

# 校验 JNI 导出符号：readelf 取 NDK 自带的那个，并按宿主补后缀（Windows 上是 .exe 且不在 PATH）。
# `llvm-readelf … | grep … || true` 在工具缺失时会静默空跑，"校验"于是假装通过 ——
# 故缺工具与零符号都硬失败，不吞错。
# NDK 根目录有三种合法写法：直接指版本目录、指 SDK 的 `ndk/` 父目录、只设了 ANDROID_HOME。
# 只按其中一种找会在另外两种上误报"缺工具"，所以三种都收进候选再挑真有工具链的那个；
# Windows 的反斜杠路径在 MSYS 下 glob 匹配不上，先规范化成正斜杠再找。
to_posix() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -u "$1" 2>/dev/null || printf '%s' "$1";
  else printf '%s' "$1"; fi
}
NDK="$(to_posix "${ANDROID_NDK_HOME:-}")"
SDK="$(to_posix "${ANDROID_HOME:-}")"
READELF=""
for root in "$NDK" "$NDK"/*/ "$SDK/ndk"/*/; do
  [ -d "$root/toolchains/llvm/prebuilt" ] || continue
  for c in "$root"/toolchains/llvm/prebuilt/*/bin/llvm-readelf \
           "$root"/toolchains/llvm/prebuilt/*/bin/llvm-readelf.exe; do
    [ -x "$c" ] && { READELF="$c"; break 2; }
  done
done
[ -n "$READELF" ] \
  || { echo "❌ 未找到 NDK llvm-readelf，无法校验 JNI 导出符号（ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-未设置} ANDROID_HOME=${ANDROID_HOME:-未设置}）"; exit 1; }
SYMS_RE='Java_com_linkx_app|linkx_ffi|linkx_version'
for SO in "${SOS[@]}"; do
  ABI_DIR="$(basename "$(dirname "$SO")")"
  # 字节层断言 ABI 真的是那套 ABI：cargo-ndk 的 target→链接器映射出错时，
  # 两份 .so 会同为 64 位，包照样打得出，只有 32 位机器装上才崩——这里是那道闸。
  HDR="$("$READELF" -h "$SO")"
  want_class=""; want_machine=""
  case "$ABI_DIR" in
    arm64-v8a)   want_class=ELF64; want_machine=AArch64 ;;
    armeabi-v7a) want_class=ELF32; want_machine=ARM ;;
  esac
  if [ -n "$want_class" ]; then
    echo "$HDR" | grep -q "Class:.*$want_class" \
      || { echo "❌ $SO 不是 $want_class：$ABI_DIR 这套 ABI 的产物形态不对"; exit 1; }
    echo "$HDR" | grep -q "Machine:.*$want_machine" \
      || { echo "❌ $SO 不是 $want_machine 机器码"; exit 1; }
  fi
  SYMS="$("$READELF" --dyn-syms "$SO" | grep -cE "$SYMS_RE" || true)"
  [ "${SYMS:-0}" -gt 0 ] \
    || { echo "❌ $SO 未导出任何 JNI/FFI 符号（装机后 NativeCore.loadLibrary 必崩）"; exit 1; }
  echo "  ✓ $ABI_DIR：$want_class / $want_machine，导出符号命中 $SYMS 个（Java_com_linkx_app* + linkx_*）"
done