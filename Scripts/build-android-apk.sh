#!/usr/bin/env bash
# Android 安装包（.apk）构建 —— Rust Core .so + Gradle + JDK 21 → 归档 Release/Android/
#
# 用法：
#   bash Scripts/build-android-apk.sh              # debug APK（联调首选：可 logcat、装机即用）
#   bash Scripts/build-android-apk.sh --release    # release APK（R8 压缩）+ 自签（无商业证书）
#
# 版本要求：仓库锁定 Gradle 8.7 + AGP 8.5.2，而 AGP 8.5.2 不接受 JDK 25 及以上，
#   构建必须拿到 JDK 21。PATH 上的 gradle 未必是工程锁定的那个发行版，
#   所以 Gradle 与 JDK 都按下文的顺序显式解析，不依赖环境里恰好是什么。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="debug"
[ "${1:-}" = "--release" ] && MODE="release"

echo "== 1) 环境检查 =="
# 跨宿主解析可执行文件：Linux 上 JDK / build-tools 的入口无后缀，Windows 上是 .exe / .bat。
# 只按其中一种硬编码，另一种宿主上就会报"缺 JDK 21"，使用者只好绕过脚本手敲命令。
find_exe() {
  local c
  for c in "$1" "$1.exe" "$1.bat" "$1.cmd"; do
    [ -f "$c" ] || continue
    # Windows 的 .bat/.cmd 在 MSYS 下常无 exec 位（权限由 ACL 推导），但仍可经 cmd 执行，
    # 故这两个后缀只看存在性、不强求 -x。
    case "$c" in
      *.bat|*.cmd) printf '%s' "$c"; return 0 ;;
    esac
    [ -x "$c" ] && { printf '%s' "$c"; return 0; }
  done
  return 1
}
JAVA_HOME_21="${JAVA_HOME_21:-${JAVA_HOME:-/usr/lib/jvm/java-21-openjdk-amd64}}"
JAVA_CMD="$(find_exe "$JAVA_HOME_21/bin/java")" \
  || { echo "❌ 缺 JDK 21（$JAVA_HOME_21/bin/java[.exe]）；可设 JAVA_HOME_21 指定路径"; exit 1; }
KEYTOOL="$(find_exe "$JAVA_HOME_21/bin/keytool")" \
  || { echo "❌ 缺 keytool（$JAVA_HOME_21/bin）"; exit 1; }
# Gradle 解析顺序：GRADLE_BIN 环境变量 → PATH 上的 gradle → 仓库 wrapper → 仓库内的本机发行版布局
if [ -z "${GRADLE_BIN:-}" ]; then
  if command -v gradle >/dev/null 2>&1; then GRADLE_BIN="$(command -v gradle)"
  elif [ -x "$ROOT/Platforms/Android/gradlew" ]; then GRADLE_BIN="$ROOT/Platforms/Android/gradlew"
  else GRADLE_BIN="$(ls -d "$ROOT"/Tools/Toolchain/Gradle/wrapper/dists/gradle-8.7-bin/*/gradle-8.7/bin/gradle 2>/dev/null | head -1)"
  fi
fi
[ -n "$GRADLE_BIN" ] || { echo "❌ 找不到 Gradle：装一个并放进 PATH，或设 GRADLE_BIN 指向可执行文件"; exit 1; }
# build-tools 解析顺序：BUILD_TOOLS 环境变量 → SDK 里的最高版本 → 仓库内的本机布局
if [ -z "${BUILD_TOOLS:-}" ]; then
  BT="${ANDROID_HOME:+$ANDROID_HOME/build-tools}"
  [ -d "$BT" ] || BT="$ROOT/Tools/Toolchain/Android-SDK/build-tools"
  BUILD_TOOLS="$(ls -d "$BT"/* 2>/dev/null | sort -V | tail -1)"
fi
BT="${BUILD_TOOLS:-/nonexistent}"
APKSIGNER="$(find_exe "$BT/apksigner")" || { echo "❌ 缺 apksigner（$BT）"; exit 1; }
ZIPALIGN="$(find_exe "$BT/zipalign")" || { echo "❌ 缺 zipalign（$BT）"; exit 1; }
echo "  JDK 21（$JAVA_CMD）"
echo "  Gradle 8.7 / apksigner（$APKSIGNER）/ zipalign 就绪"

echo
echo "== 2) Rust Core .so（arm64-v8a） =="
SO="Target/android-jni/arm64-v8a/liblinkx_core.so"
# 陈旧产物陷阱：DEBUGD=1 却复用了不带控制面的 .so，会产出"看着是调试包、控制面起不来"的 APK。
# 故不靠环境变量猜，直接验 .so 里有没有那个符号。
SO_HAS_DEBUGD=0
[ -f "$SO" ] && grep -qa 'nativeDebugdStart' "$SO" && SO_HAS_DEBUGD=1
NEED_DEBUGD=0
# 调试变体按定义就要带控制面：只认显式的 DEBUGD=1 时，"MODE=debug + 忘了给 DEBUGD +
# 沿用旧的无控制面 .so"会产出一个名字叫 debug、`/state` 全空、脚本驱动不了的 APK。
# 故 NEED_DEBUGD 由 MODE 决定，不让调用者额外记环境变量。
[ "$MODE" = "debug" ] && NEED_DEBUGD=1
[ "${DEBUGD:-0}" = "1" ] && NEED_DEBUGD=1
if [ "$SO_HAS_DEBUGD" != "$NEED_DEBUGD" ]; then
  echo "  ⚠ 已有 .so 的 agent-debug 状态（$SO_HAS_DEBUGD）与本次要求（$NEED_DEBUGD）不符 → 强制重建"
  REBUILD_CORE=1
fi
# 只验"有没有控制面符号"挡不住另一种陈旧：**Rust 源码改了但没重编**——
# APK 里 Kotlin 已经调新 JNI 函数、.so 里却还没有那个符号，而 Kotlin 侧 runCatching
# 会把 UnsatisfiedLinkError 吞成"返回 0"，表现成一个和功能本身毫无关系的假故障。
# 故比符号之外再比一次时间戳：任何 .rs/.proto/Cargo.toml 比 .so 新就强制重建。
if [ -f "$SO" ] && [ "${REBUILD_CORE:-0}" != "1" ]; then
  NEWER="$(find Crates Proto -type f \( -name '*.rs' -o -name '*.proto' -o -name 'Cargo.toml' \) -newer "$SO" 2>/dev/null | head -1)"
  if [ -n "$NEWER" ]; then
    echo "  ⚠ .so 落后于源码（$NEWER）→ 强制重建 Rust Core"
    REBUILD_CORE=1
  fi
fi

if [ -f "$SO" ] && [ "${REBUILD_CORE:-0}" != "1" ]; then
  echo "  复用已有 $SO（REBUILD_CORE=1 可强制重建）"
else
  # DEBUGD 必须**跟着本次要求传下去**：核心脚本读的是环境变量 DEBUGD，
  # 只置 REBUILD_CORE 就调用的话，"要求调试变体"会被重建成一份**仍然没有控制面**的 .so，
  # 第 2 步那道符号闸门也会被白触发一次。
  DEBUGD="$NEED_DEBUGD" bash Scripts/build-android-core.sh
fi
# 事后断言（检查器自己也要被验）：**产出的 .so 真的符合本次要求**才算过。
# 只看脚本有没有报错是不够的——它报错的方式是"成功产出一个错的变体"。
FINAL_HAS_DEBUGD=0
grep -qa 'nativeDebugdStart' "$SO" && FINAL_HAS_DEBUGD=1
if [ "$FINAL_HAS_DEBUGD" != "$NEED_DEBUGD" ]; then
  echo "❌ .so 的控制面状态（$FINAL_HAS_DEBUGD）与本次要求（$NEED_DEBUGD）不符，停在这里"
  exit 1
fi
echo "  ✓ .so 控制面状态与本次要求一致（$NEED_DEBUGD）"

echo
echo "== 3) Gradle 组装 $MODE APK =="
TASK="assembleDebug"
[ "$MODE" = "release" ] && TASK="assembleRelease"
# 内存紧张的机器上：gradle.properties 的 parallel + 默认 worker 数会让 R8 阶段 OOM，
# Gradle daemon 被内核杀掉（"daemon disappeared unexpectedly"）。
# 故显式关闭并行并把 worker 限为 1；单模块构建无并行收益，属无副作用约束。
# --offline 是给本机的（依赖已在本地缓存，联网检索只会变慢）；CI 是冷缓存，用 GRADLE_ONLINE=1 放行。
GRADLE_NET="--offline"
[ "${GRADLE_ONLINE:-0}" = "1" ] && GRADLE_NET=""
( cd Platforms/Android && JAVA_HOME="$JAVA_HOME_21" PATH="$JAVA_HOME_21/bin:$PATH" \
    "$GRADLE_BIN" -p . "$TASK" --no-daemon --no-parallel $GRADLE_NET \
    -Dorg.gradle.workers.max=1 -q )
echo "  ✅ gradle $TASK 完成"

echo
echo "== 4) 取产物 =="
VERSION="$(sed -n 's/^[[:space:]]*versionName = "\(.*\)"/\1/p' Platforms/Android/app/build.gradle.kts | head -1)"
VERSION="${VERSION:-0.0.0}"
mkdir -p Release/Android
# debug 与 release 用同一把测试密钥签名（由本脚本在构建机上按需生成，不入库）。
# AGP 给 debug 包默认用 ~/.android/debug.keystore，与交付包那把不同 → `adb install -r`
# 直接报 INSTALL_FAILED_UPDATE_INCOMPATIBLE，只能先卸载，App 私有数据连同配对关系一起没，
# 于是"装一次调试包就得重新配对一次"。签名统一后两种包可互相覆盖安装、配对完好。
KS="${LINKX_TEST_KEYSTORE:-$ROOT/Tools/Keys/linkx-test.jks}"
if [ ! -f "$KS" ]; then
  mkdir -p "$(dirname "$KS")"
  "$KEYTOOL" -genkeypair -v -keystore "$KS" -alias linkx-test \
    -keyalg RSA -keysize 2048 -validity 10000 \
    -storepass linkx-test -keypass linkx-test \
    -dname "CN=LinkX Test, OU=Test, O=LinkX Project, L=-, ST=-, C=CN" >/dev/null 2>&1
  echo "  已生成测试密钥 $KS（后续版本复用，保证覆盖安装签名一致）"
fi
if [ "$MODE" = "debug" ]; then
  APK_IN="Platforms/Android/app/build/outputs/apk/debug/app-debug.apk"
  APK_OUT="Release/Android/LinkX-$VERSION-debug.apk"
  echo "  --- 用本机测试密钥重签（与交付包同一把，覆盖安装不丢配对） ---"
  ALIGNED="Target/android-jni/app-debug-aligned.apk"
  # -p 不可省：未压缩的 .so 必须**按页对齐**才能被 mmap。缺它时安装包在部分机型上直接报
  # `INSTALL_FAILED_INVALID_APK: Failed to extract native libraries, res=-2`——
  # 带控制面的 debug .so 更大、更容易把条目推到非页边界上，于是"调试包装不上、交付包装得上"。
  "$ZIPALIGN" -f -p 4 "$APK_IN" "$ALIGNED"
  "$APKSIGNER" sign --ks "$KS" --ks-key-alias linkx-test \
    --ks-pass pass:linkx-test --key-pass pass:linkx-test \
    --out "$APK_OUT" "$ALIGNED"
else
  UNSIGNED="Platforms/Android/app/build/outputs/apk/release/app-release-unsigned.apk"
  [ -f "$UNSIGNED" ] || { echo "❌ 未找到 $UNSIGNED（release 是否配置了签名？）"; exit 1; }
  echo "  --- 自签（无商业证书；测试密钥，勿用于正式发布） ---"
  ALIGNED="Target/android-jni/app-release-aligned.apk"
  APK_OUT="Release/Android/LinkX-$VERSION-release.apk"
  "$ZIPALIGN" -f -p 4 "$UNSIGNED" "$ALIGNED"
  "$APKSIGNER" sign --ks "$KS" --ks-key-alias linkx-test \
    --ks-pass pass:linkx-test --key-pass pass:linkx-test \
    --out "$APK_OUT" "$ALIGNED"
fi

echo
echo "== 5) 静态校验 =="
# debug/release 都只验签（apksigner.bat 需要 JAVA_HOME）。
JAVA_HOME="$JAVA_HOME_21" PATH="$JAVA_HOME_21/bin:$PATH" "$APKSIGNER" verify --print-certs "$APK_OUT" | head -6
# 页对齐自检（守门人）：不通过就意味着这包在真机上可能根本装不上，
# 而 `apksigner verify` 查不出它——签名合法、包也完整，只是未压缩的 .so 没落在页边界上。
"$ZIPALIGN" -c -p 4 "$APK_OUT" || { echo "❌ APK 未按要求页对齐（zipalign -c -p 失败）"; exit 1; }
echo "  ✓ 页对齐校验通过"
unzip -l "$APK_OUT" | grep -E 'classes.dex|lib/arm64-v8a/liblinkx_core.so' || { echo "❌ APK 缺 dex 或 .so"; exit 1; }
# 交付（release）APK 里那份 .so 必须零调试残留，且必须真的导出当前 Kotlin 用到的 JNI
# 符号——否则"Kotlin 调的符号在旧 .so 里不存在"会被 runCatching 咽掉，看不出问题。
# debug APK 的控制面由第 2 步那道"符号闸门"保证（MODE=debug ⇒ 必须重建出带 debugd 的 .so），
# 所以这里只查交付版——交付版带残留就是发布事故。
if [ "$MODE" = "release" ]; then
  bash Scripts/check-release-clean.sh --apk-so "$APK_OUT"
fi
file "$APK_OUT"

echo
echo "== 6) 归档 + SHA-256 =="
( cd Release/Android && sha256sum "$(basename "$APK_OUT")" > "$(basename "$APK_OUT").sha256" )
ls -lh "$APK_OUT" | awk '{print "  " $5 "  " $9}'
cat "Release/Android/$(basename "$APK_OUT").sha256"
echo "✅ Android APK 交付完成（$MODE）"