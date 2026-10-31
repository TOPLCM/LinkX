#!/usr/bin/env bash
# Android 安装包（.apk）构建 —— Rust Core .so + Gradle + JDK 21 → 归档 Release/Android/
# 用法：bash Scripts/build-android-apk.sh [--release]
# 版本要求：仓库锁定 Gradle 8.7 + AGP 8.5.2（不接受 JDK 25+），构建必须拿到 JDK 21；
#   Gradle 与 JDK 都显式解析，不依赖环境里恰好是什么。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="debug"
[ "${1:-}" = "--release" ] && MODE="release"

echo "== 1) 环境检查 =="
# 跨宿主解析可执行文件：Linux 上入口无后缀，Windows 上是 .exe/.bat，只硬编码一种会让另一种宿主误报"缺 JDK"。
find_exe() {
  local c
  for c in "$1" "$1.exe" "$1.bat" "$1.cmd"; do
    [ -f "$c" ] || continue
    # Windows 的 .bat/.cmd 在 MSYS 下常无 exec 位，但可经 cmd 执行，这两个后缀只看存在性。
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
  else GRADLE_BIN="$(ls -d "$ROOT"/LOCAL_TOOLCHAIN/Gradle/wrapper/dists/gradle-8.7-bin/*/gradle-8.7/bin/gradle 2>/dev/null | head -1)"
  fi
fi
[ -n "$GRADLE_BIN" ] || { echo "❌ 找不到 Gradle：装一个并放进 PATH，或设 GRADLE_BIN 指向可执行文件"; exit 1; }
# build-tools 解析顺序：BUILD_TOOLS 环境变量 → SDK 里的最高版本 → 仓库内的本机布局
if [ -z "${BUILD_TOOLS:-}" ]; then
  BT="${ANDROID_HOME:+$ANDROID_HOME/build-tools}"
  [ -d "$BT" ] || BT="$ROOT/LOCAL_TOOLCHAIN/Android-SDK/build-tools"
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
# 陈旧产物陷阱：DEBUGD=1 却复用不带控制面的 .so，会产出"看着是调试包、控制面起不来"的 APK，故直接验符号。
SO_HAS_DEBUGD=0
[ -f "$SO" ] && grep -qa 'nativeDebugdStart' "$SO" && SO_HAS_DEBUGD=1
NEED_DEBUGD=0
# 调试变体按定义就要带控制面：NEED_DEBUGD 由 MODE 决定，不让调用者额外记环境变量。
[ "$MODE" = "debug" ] && NEED_DEBUGD=1
[ "${DEBUGD:-0}" = "1" ] && NEED_DEBUGD=1
if [ "$SO_HAS_DEBUGD" != "$NEED_DEBUGD" ]; then
  echo "  ⚠ 已有 .so 的 agent-debug 状态（$SO_HAS_DEBUGD）与本次要求（$NEED_DEBUGD）不符 → 强制重建"
  REBUILD_CORE=1
fi
# 只验符号挡不住"Rust 源码改了但没重编"——Kotlin 调新 JNI 函数、旧 .so 里没有该符号，
# 而 runCatching 会把 UnsatisfiedLinkError 吞成"返回 0"，表现成无关的假故障。故再比一次时间戳。
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
  # DEBUGD 必须跟着本次要求传下去，否则"要求调试变体"会重建成一份仍然没有控制面的 .so。
  DEBUGD="$NEED_DEBUGD" bash Scripts/build-android-core.sh
fi
# 事后断言（检查器自己也要被验）：产出的 .so 真的符合本次要求才算过。
FINAL_HAS_DEBUGD=0
grep -qa 'nativeDebugdStart' "$SO" && FINAL_HAS_DEBUGD=1
if [ "$FINAL_HAS_DEBUGD" != "$NEED_DEBUGD" ]; then
  echo "❌ .so 的控制面状态（$FINAL_HAS_DEBUGD）与本次要求（$NEED_DEBUGD）不符，停在这里"
  exit 1
fi
echo "  ✓ .so 控制面状态与本次要求一致（$NEED_DEBUGD）"

echo
echo "== 3) 许可清单进 APK =="
# APK 也是一种分发形式：不带许可清单 = 义务未履行（登记见仓库根 THIRD_PARTY_LICENSES.md）。
# 先清空再复制，保证每次构建只取决于当前仓库状态。
LIC_DIR="Platforms/Android/app/src/main/assets/licenses"
if [ -d "$LIC_DIR" ]; then
  find "$LIC_DIR" -maxdepth 1 -type f -delete
fi
mkdir -p "$LIC_DIR"
cp -f LICENSE "$LIC_DIR/LICENSE"
cp -f THIRD_PARTY_LICENSES.md "$LIC_DIR/THIRD_PARTY_LICENSES.md"
cp -f Licenses/localsend/LICENSE "$LIC_DIR/LICENSE-localsend.txt"
cp -f Licenses/localsend/NOTICE "$LIC_DIR/NOTICE-localsend.txt"
cp -f Licenses/localsend/PORTING.md "$LIC_DIR/PORTING-localsend.md"
# 判据：每份都得存在且非空——"复制成功"不保证源文件非空，而空清单等于没随附。
for f in LICENSE THIRD_PARTY_LICENSES.md LICENSE-localsend.txt NOTICE-localsend.txt PORTING-localsend.md; do
  [ -s "$LIC_DIR/$f" ] || { echo "❌ 许可清单缺 $f（源文件空或缺失）"; exit 1; }
done
echo "  ✓ 许可清单已就位（$(ls "$LIC_DIR" | wc -l) 份）"

echo
echo "== 4) Gradle 组装 $MODE APK =="
TASK="assembleDebug"
[ "$MODE" = "release" ] && TASK="assembleRelease"
# 内存紧张的机器上 R8 阶段会 OOM（daemon disappeared unexpectedly），故关并行、worker 限 1。
# --offline 给本机用（依赖已缓存）；CI 冷缓存时用 GRADLE_ONLINE=1 放行。
GRADLE_NET="--offline"
[ "${GRADLE_ONLINE:-0}" = "1" ] && GRADLE_NET=""
( cd Platforms/Android && JAVA_HOME="$JAVA_HOME_21" PATH="$JAVA_HOME_21/bin:$PATH" \
    "$GRADLE_BIN" -p . "$TASK" --no-daemon --no-parallel $GRADLE_NET \
    -Dorg.gradle.workers.max=1 -q )
echo "  ✅ gradle $TASK 完成"

echo
echo "== 5) 取产物 =="
VERSION="$(sed -n 's/^[[:space:]]*versionName = "\(.*\)"/\1/p' Platforms/Android/app/build.gradle.kts | head -1)"
VERSION="${VERSION:-0.0.0}"
mkdir -p Release/Android
# debug 与 release 用同一把测试密钥签名（构建机上按需生成，不入库）：否则 `adb install -r`
# 直接报 INSTALL_FAILED_UPDATE_INCOMPATIBLE，只能卸载重来，App 私有数据连同配对关系一起丢。
KS="${LINKX_TEST_KEYSTORE:-$ROOT/LOCAL_KEYS/linkx-test.jks}"
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
  # -p 不可省：未压缩的 .so 必须按页对齐才能被 mmap，缺它时部分机型直接报 INSTALL_FAILED_INVALID_APK。
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
echo "== 6) 静态校验 =="
# debug/release 都只验签（apksigner.bat 需要 JAVA_HOME）。
JAVA_HOME="$JAVA_HOME_21" PATH="$JAVA_HOME_21/bin:$PATH" "$APKSIGNER" verify --print-certs "$APK_OUT" | head -6
# 页对齐自检：签名合法、包也完整不代表装得上，`apksigner verify` 查不出未页对齐的 .so。
"$ZIPALIGN" -c -p 4 "$APK_OUT" || { echo "❌ APK 未按要求页对齐（zipalign -c -p 失败）"; exit 1; }
echo "  ✓ 页对齐校验通过"
unzip -l "$APK_OUT" | grep -E 'classes.dex|lib/arm64-v8a/liblinkx_core.so' || { echo "❌ APK 缺 dex 或 .so"; exit 1; }
# 许可清单必须真的在包里；grep 不用 -q（它一命中就关管道，unzip 收 SIGPIPE，pipefail 下会误判失败）。
LIC_IN_APK="$(unzip -l "$APK_OUT" | grep -cE 'assets/licenses/' || true)"
[ "${LIC_IN_APK:-0}" -ge 5 ] || { echo "❌ APK 里只有 ${LIC_IN_APK:-0} 份许可清单（应为 5 份）"; exit 1; }
echo "  ✓ 许可清单已在包内（${LIC_IN_APK} 份）"
# 交付版 APK 里的 .so 必须零调试残留，且必须真的导出当前 Kotlin 用到的 JNI 符号——
# 否则"Kotlin 调的符号在旧 .so 里不存在"会被 runCatching 咽掉。debug 版由第 2 步的符号闸门保证。
if [ "$MODE" = "release" ]; then
  bash Scripts/check-release-clean.sh --apk-so "$APK_OUT"
fi
file "$APK_OUT"

echo
echo "== 7) 归档 + SHA-256 =="
( cd Release/Android && sha256sum "$(basename "$APK_OUT")" > "$(basename "$APK_OUT").sha256" )
ls -lh "$APK_OUT" | awk '{print "  " $5 "  " $9}'
cat "Release/Android/$(basename "$APK_OUT").sha256"
echo "✅ Android APK 交付完成（$MODE）"