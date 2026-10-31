#!/usr/bin/env bash
# Windows 安装包（.msi）构建 —— 交叉编译 linkx.exe + 打 MSI。
# 打包引擎只有一条：**WiX Toolset v5**（`Platforms/Windows/installers/LinkX-v4.wxs`）。
# 原先还有一条 Linux/wixl 链路（配 `installers/LinkX.wxs`），那份源自陈"携带 6 处已知向导缺陷"、
# 从未在真机上走过安装向导，开源前已随它一起下线（见 `内部文档/计划.md`）。
#
# 流程：环境检查 → 交叉编译 linkx.exe → 注入版本生成 .wxs → 打包 msi → 静态校验 → 归档
# 产物：<OUT_DIR>/LinkX-<version>-x64.msi(.sha256)   （OUT_DIR 默认 Release/Windows）
# 用法：bash Scripts/build-msi-windows.sh
# 可调环境变量：SKIP_EXE_BUILD=1（复用已有 exe）/ OUT_DIR=<dir>（产物目录）/ WIX_V4_WXS=<file>（换 .wx）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SKIP_EXE_BUILD="${SKIP_EXE_BUILD:-0}"
OUT_DIR="${OUT_DIR:-Release/Windows}"
EXE="Target/x86_64-pc-windows-gnu/release/linkx.exe"

die() { echo "❌ $*" >&2; exit 1; }

echo "== 1) 环境检查 =="
# WiX Toolset 反过来在非 Windows 上产不出 MSI，故这条链路只在 Windows 宿主上跑。
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*|CYGWIN*|Windows*) ;;
  *) die "MSI 只在 Windows 宿主上构建（WiX Toolset 在 Linux 上产不出 MSI）；请用 Windows 机器或 CI 的 windows job" ;;
esac
command -v wix >/dev/null || die "未找到 wix（dotnet tool install --tool-path LOCAL_WIX/wix-5.0.2 wix --version 5.0.2）"
if [ "$SKIP_EXE_BUILD" != "1" ]; then
  command -v x86_64-w64-mingw32-gcc >/dev/null \
    || die "缺 MinGW 交叉编译器（Debian/Ubuntu: apt install mingw-w64）"
fi
echo "  工具链就绪（引擎 wix）"

echo
echo "== 2) 交叉编译 Windows 壳（release） =="
if [ "$SKIP_EXE_BUILD" = "1" ]; then
  echo "  SKIP_EXE_BUILD=1，复用已有 exe"
else
  bash Scripts/build-mingw-windows.sh
fi
[ -f "$EXE" ] || die "未找到 $EXE（去掉 SKIP_EXE_BUILD 先构建）"

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
sed "s/@VERSION@/$VERSION/g" "$WXS_SRC" > "$WXS_GEN"
echo "  版本 $VERSION：$WXS_SRC → $WXS_GEN"

echo
echo "== 4) 打包 MSI（x64） =="
OUT="Target/msi/LinkX-$VERSION-x64.msi"
command -v wix >/dev/null || die "未找到 wix（dotnet tool install -g wix）"
echo "  引擎 = WiX Toolset（构建已注入版本的 $WXS_GEN）"
# -ext 带 util 扩展（CloseApplication 只有它有）。优先指到本机磁盘上那份（LOCAL_WIX/ 是安装副本，
# 不入库）：只写包名时 WiX 会去 NuGet 现取现装进 ~/.wix/extensions，出包这一步就悄悄依赖网络。
UTIL_EXT="$(ls LOCAL_WIX/extensions/WixToolset.Util.wixext/*/wixext5/WixToolset.Util.wixext.dll 2>/dev/null | head -1)"
[ -n "$UTIL_EXT" ] || UTIL_EXT=WixToolset.Util.wixext
wix build "$WXS_GEN" -arch x64 -ext "$UTIL_EXT" -o "$OUT"
echo "  ✅ $OUT"

echo
echo "== 5) 静态校验 =="
# 必须在这里真的执行：只在文档里写"记得跑一次"等于没有门禁
bash Scripts/check-release-clean.sh "$EXE"
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
cp -f "$OUT" "$OUT_DIR/LinkX-$VERSION-x64.msi"
( cd "$OUT_DIR" && sha256sum "LinkX-$VERSION-x64.msi" > "LinkX-$VERSION-x64.msi.sha256" )
ls -lh "$OUT_DIR/LinkX-$VERSION-x64.msi"
cat "$OUT_DIR/LinkX-$VERSION-x64.msi.sha256"
echo "✅ Windows MSI 交付完成（未签名；SmartScreen 需「更多信息 → 仍要运行」）"

# 只提示不阻断：装之前必须看见本机注册了几份 LinkX，判据见 check-install-single.sh
echo
echo "== 7) 本机安装份数核对 =="
if bash "$ROOT/Scripts/check-install-single.sh"; then
  echo "  （装完这份 MSI 后建议再跑一次：bash Scripts/check-install-single.sh）"
else
  echo "  ⚠️  先按上面的说明清掉多余那份再装，否则你会拿到两套身份、又要重新配对。"
fi