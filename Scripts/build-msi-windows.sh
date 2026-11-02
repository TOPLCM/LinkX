#!/usr/bin/env bash
# Windows 安装包（.msi）构建 —— 编出对应架构的 linkx.exe + 打 MSI。
# 打包引擎只有一条：**WiX Toolset v5**（`Platforms/Windows/installers/LinkX-v4.wxs`）；
# 旧的 Linux/wixl 那条链路已在开源前下线，这里不再有第二个打包引擎。
#
# 产物：<OUT_DIR>/LinkX-<version>-<arch>.msi(.sha256)   （OUT_DIR 默认 Release/Windows）
# 用法：bash Scripts/build-msi-windows.sh                     # 默认 x64
#       ARCH=arm64 bash Scripts/build-msi-windows.sh          # 原生 ARM64（需 MSVC ARM64 工具链）
# 可调环境变量：ARCH=x64|arm64（默认 x64）/ SKIP_EXE_BUILD=1（复用已有 exe）/
#              OUT_DIR=<dir>（产物目录）/ WIX_V4_WXS=<file>（换 .wx）
#
# 两个架构是两条编译链，不是切个 target 就完事：x64 用 MinGW（任何宿主都能交叉编），
# arm64 用 MSVC + SDK 的 ARM64 库与 rc.exe（只能 Windows 宿主）。两套都留着的理由见 Docs/Build.md。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

die() { echo "❌ $*" >&2; exit 1; }

ARCH="${ARCH:-x64}"
case "$ARCH" in
  x64)   TRIPLE="x86_64-pc-windows-gnu";   BUILD_SCRIPT="Scripts/build-mingw-windows.sh" ;;
  arm64) TRIPLE="aarch64-pc-windows-msvc"; BUILD_SCRIPT="Scripts/build-msvc-windows.sh" ;;
  *) die "ARCH=$ARCH 不认识，只支持 x64 / arm64" ;;
esac

SKIP_EXE_BUILD="${SKIP_EXE_BUILD:-0}"
OUT_DIR="${OUT_DIR:-Release/Windows}"
EXE="Target/$TRIPLE/release/linkx.exe"

die() { echo "❌ $*" >&2; exit 1; }

echo "== 1) 环境检查 =="
# WiX Toolset 反过来在非 Windows 上产不出 MSI，故这条链路只在 Windows 宿主上跑。
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*|CYGWIN*|Windows*) ;;
  *) die "MSI 只在 Windows 宿主上构建（WiX Toolset 在 Linux 上产不出 MSI）；请用 Windows 机器或 CI 的 windows job" ;;
esac
command -v wix >/dev/null || die "未找到 wix（dotnet tool install --tool-path LOCAL_WIX/wix-5.0.2 wix --version 5.0.2）"
if [ "$SKIP_EXE_BUILD" != "1" ] && [ "$ARCH" = "x64" ]; then
  command -v x86_64-w64-mingw32-gcc >/dev/null \
    || die "缺 MinGW 交叉编译器（Debian/Ubuntu: apt install mingw-w64）"
fi
# arm64 这条不预探工具链：MSVC 的 link.exe 与 SDK 的 ARM64 库要靠 rustc 自己找，
# 提前查只会查出一个"没找到 cl.exe"的假结论（本机装了 VS 但没开开发者提示符时也是这样）。
echo "  工具链就绪（引擎 wix，架构 $ARCH → 目标 $TRIPLE）"

echo
echo "== 2) 编译 Windows 壳（release，$ARCH） =="
if [ "$SKIP_EXE_BUILD" = "1" ]; then
  echo "  SKIP_EXE_BUILD=1，复用已有 exe"
else
  bash "$BUILD_SCRIPT"
fi
[ -f "$EXE" ] || die "未找到 $EXE（去掉 SKIP_EXE_BUILD 先构建）"
if [ "$SKIP_EXE_BUILD" = "1" ] && [ "${GITHUB_ACTIONS:-}" != "true" ]; then
  # SKIP_EXE_BUILD 是"旧 exe 装进新 MSI"的入口：本机复用一份没重编的 exe，包名、台账、
  # 安装器属性页都写着新版本，本体还是上一版 —— 表现成"升级了但行为没变"的假故障。
  # CI 上不查：那里的 exe 由同一条流水线的上一个 job 从**同一个 commit** 编出来并走产物传递，
  # 工件时间戳必然早于本 job 的 checkout 时间，按时间判活只会天天误报。
  STALE="$(find Crates Platforms Proto -type f \( -name '*.rs' -o -name '*.proto' -o -name 'Cargo.toml' -o -name '*.rc' \) \
           -newer "$EXE" 2>/dev/null | head -1)"
  [ -z "$STALE" ] || die "SKIP_EXE_BUILD=1，但 $EXE 落后于源码（$STALE）→ 先去掉该变量重编"
fi

echo
echo "== 3) 生成 .wxs（注入版本） =="
# 版本单一源：workspace Cargo.toml 的 [workspace.package] version
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || die "无法从 Cargo.toml 读取版本号"
mkdir -p Target/msi
# 只有一份安装源：自定义向导（中文）完整，只有它出正式包
WXS_SRC="${WIX_V4_WXS:-Platforms/Windows/installers/LinkX-v4.wxs}"
WXS_GEN="Target/msi/LinkX-v4.wxs"
[ -f "$WXS_SRC" ] || die "找不到 .wxs 源：$WXS_SRC"
# 模板里必须还留着占位符：反过来只查"生成件里有没有残留 @EXE@"是漏的 —— 有人把路径改回写死，
# sed 就空转，生成件干干净净、判据全绿，两个架构又共用同一个 exe（0.5.3 那次就是这么翻车的）。
if ! grep -q '@EXE@' "$WXS_SRC"; then die "$WXS_SRC 里没有 @EXE@ 占位符：本体路径被写死了，两个架构会共用同一个 exe"; fi
sed -e "s/@VERSION@/$VERSION/g" -e "s|@EXE@|$EXE|g" "$WXS_SRC" > "$WXS_GEN"
if grep -q '@EXE@' "$WXS_GEN"; then die "$WXS_GEN 里 @EXE@ 没被替换成 $EXE —— 注入这一步没跑"; fi
echo "  版本 $VERSION、本体 $EXE：$WXS_SRC → $WXS_GEN"

echo
echo "== 4) 打包 MSI（$ARCH） =="
OUT="Target/msi/LinkX-$VERSION-$ARCH.msi"
command -v wix >/dev/null || die "未找到 wix（dotnet tool install -g wix）"
echo "  引擎 = WiX Toolset（构建已注入版本的 $WXS_GEN，-arch $ARCH）"
# -ext 带 util 扩展（CloseApplication 只有它有）。优先指到本机磁盘上那份（LOCAL_WIX/ 是安装副本，
# 不入库）：只写包名时 WiX 会去 NuGet 现取现装进 ~/.wix/extensions，出包这一步就悄悄依赖网络。
# ARM64 包这里不需要额外准备：扩展会按包的目标架构挑对应的原生 CA，实测 arm64 包里
# 落的是 Wix4UtilCA_A64 与 Wix4CloseApplications_A64，那道"卸载前先结束本体"的保护没掉。
UTIL_EXT="$(ls LOCAL_WIX/extensions/WixToolset.Util.wixext/*/wixext5/WixToolset.Util.wixext.dll 2>/dev/null | head -1)"
[ -n "$UTIL_EXT" ] || UTIL_EXT=WixToolset.Util.wixext
wix build "$WXS_GEN" -arch "$ARCH" -ext "$UTIL_EXT" -o "$OUT"
echo "  ✅ $OUT"

echo
echo "== 5) 静态校验 =="
# 必须在这里真的执行：只在文档里写"记得跑一次"等于没有门禁。
# 依赖图那道闸门查的必须是**这个架构的目标**：arm64 包查 gnu 目标的依赖图，
# 等于给一份没查过的产物发了绿牌。
LINKX_GATE_TARGET="$TRIPLE" bash Scripts/check-release-clean.sh "$EXE"
# 包里那份 exe 是不是刚编出来的那一份、架构对不对：上面所有校验看的都是**构建产物**，
# 没有一条看过包内的 payload —— 0.5.3 的 arm64 包就是这么带着 x64 本体过了全部判据的。
# 这条不许跳过（无 powershell 就停），因为它是唯一一条"从包里往外看"的判据。
command -v powershell >/dev/null 2>&1 || die "缺 powershell，无法校验包内 payload（这条不许跳）"
echo "  --- 包内 payload 校验（管理性解包 → 核哈希与 PE 机器码） ---"
MSI_W="$OUT"
command -v cygpath >/dev/null 2>&1 && MSI_W="$(cygpath -w "$OUT")"
powershell -NoProfile -ExecutionPolicy Bypass \
  -File "$(command -v cygpath >/dev/null 2>&1 && cygpath -w Scripts/check-msi-payload.ps1 || echo Scripts/check-msi-payload.ps1)" \
  -Msi "$MSI_W" -Exe "$(command -v cygpath >/dev/null 2>&1 && cygpath -w "$EXE" || echo "$EXE")" \
  -Arch "$ARCH" || die "包里的本体与刚编出来的那份不一致，见上面的 FAIL 行"
# 卸载/升级前的退出保护：漏了它不报错，卸载照样返回成功而本体还在跑，故按表校验
if command -v powershell >/dev/null 2>&1; then
  echo "  --- CloseApplication（卸载/升级前先退出程序）表级校验 ---"
  MSI_W="$OUT"
  command -v cygpath >/dev/null 2>&1 && MSI_W="$(cygpath -w "$OUT")"
  powershell -NoProfile -ExecutionPolicy Bypass \
    -File "$(command -v cygpath >/dev/null 2>&1 && cygpath -w Scripts/check-msi-uninstall.ps1 || echo Scripts/check-msi-uninstall.ps1)" \
    -Msi "$MSI_W" || die "卸载前的退出保护没进包，见上面的 FAIL 行"
else
  echo "  ⚠️ 本机无 powershell，CloseApplication 表级校验跳过"
fi
echo "  --- wix msi validate（ICE 全量） ---"
wix msi validate "$OUT" || echo "  （上列为 ICE 告警/错误，需逐条判定；已知 ICE20 是有意保留的缺口）"
# 表级契约（per-user 挂载 / 卸载清理 / 防火墙动作 / 版本号）原先只挂在 Linux/wixl 那条链路上，
# 而那条链路已经下线 —— 改用 Windows Installer 自带的 COM API 补回来，不依赖任何额外工具。
if command -v powershell >/dev/null 2>&1; then
  echo "  --- 表级契约校验（Windows Installer COM） ---"
  MSI_W="$OUT"
  command -v cygpath >/dev/null 2>&1 && MSI_W="$(cygpath -w "$OUT")"
  powershell -NoProfile -ExecutionPolicy Bypass \
    -File "$(command -v cygpath >/dev/null 2>&1 && cygpath -w Scripts/check-msi-tables.ps1 || echo Scripts/check-msi-tables.ps1)" \
    -Msi "$MSI_W" -Version "$VERSION" || die "MSI 表级契约不满足，见上面的 FAIL 行"
else
  echo "  ⚠️ 本机无 powershell，表级契约校验跳过"
fi

echo
echo "== 6) 归档 $OUT_DIR/ + SHA-256 =="
mkdir -p "$OUT_DIR"
cp -f "$OUT" "$OUT_DIR/LinkX-$VERSION-$ARCH.msi"
( cd "$OUT_DIR" && sha256sum "LinkX-$VERSION-$ARCH.msi" > "LinkX-$VERSION-$ARCH.msi.sha256" )
ls -lh "$OUT_DIR/LinkX-$VERSION-$ARCH.msi"
cat "$OUT_DIR/LinkX-$VERSION-$ARCH.msi.sha256"
echo "✅ Windows MSI 交付完成（$ARCH；未签名；SmartScreen 需「更多信息 → 仍要运行」）"

# 只提示不阻断：装之前必须看见本机注册了几份 LinkX，判据见 check-install-single.sh
echo
echo "== 7) 本机安装份数核对 =="
if bash "$ROOT/Scripts/check-install-single.sh"; then
  echo "  （装完这份 MSI 后建议再跑一次：bash Scripts/check-install-single.sh）"
else
  echo "  ⚠️  先按上面的说明清掉多余那份再装，否则你会拿到两套身份、又要重新配对。"
fi