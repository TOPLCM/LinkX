#!/usr/bin/env bash
# Windows 安装包（.msi）构建 —— 交叉编译 + wixl 打包，不需要 Windows/Wine

# 流程：环境检查 → 交叉编译 linkx.exe → 注入版本生成 .wxs → 打包 msi
#       → 静态校验（msiinfo/msiextract 回环）→ 归档 <OUT_DIR> + SHA-256（未签名，附校验值）
# 产物：<OUT_DIR>/LinkX-<version>-x64.msi(.sha256)   （OUT_DIR 默认 Release/Windows）

# 可调环境变量：
#   SKIP_EXE_BUILD=1              复用已有 exe（不重新交叉编译，便于调试/CI）
#   OUT_DIR=<dir>                 交付产物目录（默认 Release/Windows）
#   MSI_ENGINE=auto|wixl|wix      打包引擎（默认 auto，按宿主判定）
#   WIX_V4_WXS=<file>             选 wix 引擎时的 WiX v4/v5 源文件

# 为什么默认引擎是 wixl 而不是 WiX Toolset：WiX 在非 Windows 上产不出 MSI ——
# v4/v5/v6 的 `wix build` 把任何 <Directory Name> 判成非相对路径（WIX0389），v6+ 还要求接受收费 EULA；
# v3.14 的 candle 能在 mono 下编译，但 light 需要 Windows 的 msi.dll；wine 路线要 32 位 PE32(.NET)，
# 而多数 Linux 内核没有 IA32 支持。msitools 的 wixl 是原生链路，故默认它，
# 同时保留 MSI_ENGINE=wix 分支：在 Windows 主机上用完整 WiX Toolset 重做时提供 v4/v5 源即可。
# wixl 的能力缺口与规避办法见 Platforms/Windows/installers/LinkX.wxs 头部注释。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SKIP_EXE_BUILD="${SKIP_EXE_BUILD:-0}"
OUT_DIR="${OUT_DIR:-Release/Windows}"
MSI_ENGINE="${MSI_ENGINE:-auto}"
EXE="Target/x86_64-pc-windows-gnu/release/linkx.exe"

die() { echo "❌ $*" >&2; exit 1; }

echo "== 1) 环境检查 =="
# 引擎按宿主自动判定：wixl / msiinfo / msiextract 属 msitools，是 Linux 专有；
# WiX Toolset 反过来在非 Windows 上构建不了 MSI（见文件头）。
# 无条件要求 msitools 三件套会让本脚本在 Windows 宿主第一步就 die，逼人绕过脚本手敲命令。
ON_WINDOWS=0
case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*|Windows*) ON_WINDOWS=1 ;; esac
if [ "$MSI_ENGINE" = "auto" ]; then
  if [ "$ON_WINDOWS" = "1" ]; then MSI_ENGINE=wix; else MSI_ENGINE=wixl; fi
  echo "  宿主判定 = $([ "$ON_WINDOWS" = 1 ] && echo Windows || echo Linux)，引擎自动选 $MSI_ENGINE"
fi
if [ "$MSI_ENGINE" != "wix" ]; then
  for t in wixl msiinfo msiextract; do
    command -v "$t" >/dev/null || die "缺 $t（Debian/Ubuntu: apt install msitools wixl wixl-data）"
  done
fi
if [ "$SKIP_EXE_BUILD" != "1" ]; then
  command -v x86_64-w64-mingw32-gcc >/dev/null \
    || die "缺 MinGW 交叉编译器（Debian/Ubuntu: apt install mingw-w64）"
fi
echo "  工具链就绪（引擎 $MSI_ENGINE）"

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
# 两个引擎吃各自方言的源：wixl 源 = LinkX.wxs；WiX v4/v5 源 = LinkX-v4.wxs。
# 两者不可混用：向导缺陷只在 v4 源修过，拿 wixl 源喂 wix 引擎会装回一个带缺陷的安装器。
if [ "$MSI_ENGINE" = "wix" ]; then
  WXS_SRC="${WIX_V4_WXS:-Platforms/Windows/installers/LinkX-v4.wxs}"
  WXS_GEN="Target/msi/LinkX-v4.wxs"
else
  WXS_SRC="Platforms/Windows/installers/LinkX.wxs"
  WXS_GEN="Target/msi/LinkX.wxs"
fi
[ -f "$WXS_SRC" ] || die "找不到 .wxs 源：$WXS_SRC"
sed "s/@VERSION@/$VERSION/g" "$WXS_SRC" > "$WXS_GEN"
echo "  版本 $VERSION：$WXS_SRC → $WXS_GEN"

echo
echo "== 4) 打包 MSI（x64） =="
OUT="Target/msi/LinkX-$VERSION-x64.msi"
case "$MSI_ENGINE" in
  wix)
    command -v wix >/dev/null || die "未找到 wix（dotnet tool install -g wix）"
    echo "  引擎 = WiX Toolset（构建已注入版本的 $WXS_GEN）"
    wix build "$WXS_GEN" -arch x64 -o "$OUT"
    ;;
  wixl)
    echo "  引擎 = wixl（msitools；Linux 原生链路）"
    # 注意：必须带 --ext ui，否则 wixl 不会创建 Dialog/Control 表（自定义向导会丢失）
    wixl --ext ui -a x64 -o "$OUT" "$WXS_GEN"
    ;;
  *) die "未知 MSI_ENGINE: $MSI_ENGINE（可选 auto|wixl|wix）" ;;
esac
echo "  ✅ $OUT"

echo
echo "== 5) 静态校验 =="
# 交付构建零调试残留：这一步**必须在这里真的被执行**。只在文档里写"记得跑一次"
# 等于没有门禁——"release 不含 agent-debug"会随下一次改动悄悄失效。
bash Scripts/check-release-clean.sh "$EXE"
# msiinfo/msiextract 属 Linux 的 msitools，Windows 宿主上没有；其列级导出格式与
# Windows 侧的 msidump（只给首列）不等价，故不做"看起来一样"的替换 ——
# 缺工具时改跑 WiX 自带的 ICE 校验 + 表存在性，其余项由第 7 步真机走查承担
# （安装器一律以真机 msiexec 走查为验收手段，静态校验不单独算过）。
if ! command -v msiinfo >/dev/null 2>&1; then
  echo "  ⚠️ 本机无 msiinfo/msiextract（msitools 为 Linux 专有），以下 Linux 侧契约校验跳过："
  echo "     per-user Directory 树挂载 / RemoveFile 条目 / CAB 回环逐字节"
  echo "     → 由真机安装走查覆盖（安装→改选→卸载三回环 + 落盘/注册表/快捷方式核对）"
  echo "  --- wix msi validate（ICE 全量） ---"
  wix msi validate "$OUT" || echo "  （上列为 ICE 告警/错误，需逐条判定；已知 ICE20 是有意保留的缺口）"
  MD="Target/msi-probe/msidump/bin/Debug/net8.0/win-x64/msidump.exe"
  if [ -x "$MD" ]; then
    echo "  --- 关键表存在性（msidump） ---"
    TB="$("$MD" "$OUT" --tables | tr '\n' ' ')"
    for t in File Directory Component Feature Media Property Registry RemoveFile CustomAction \
             Dialog Control ControlEvent ControlCondition InstallExecuteSequence InstallUISequence Upgrade Icon; do
      echo "$TB" | grep -qw "$t" || die "MSI 缺表：$t"
    done
    echo "  ✅ 关键表齐备（含自定义向导 UI 表）"
    "$MD" "$OUT" "SELECT * FROM CustomAction" | sed -n '1,6p'
  else
    echo "  ⚠️ 未找到 msidump（$MD），表存在性也未校验 —— 完全依赖真机走查"
  fi
else
msiinfo suminfo "$OUT" | sed -n '1,12p'
echo "  --- tables ---"
msiinfo tables "$OUT" | tr '\n' ' '
echo
for t in File Directory Component Feature Media Property Registry RemoveFile CustomAction Shortcut \
         Dialog Control ControlEvent ControlCondition InstallExecuteSequence InstallUISequence Upgrade Icon; do
  msiinfo tables "$OUT" | grep -qx "$t" || die "MSI 缺表：$t"
done
echo "  ✅ 关键表齐备（含自定义向导 UI 表）"

echo "  --- per-user 校验（不得出现 ALLUSERS，目录须落在 LocalAppData） ---"
if msiinfo export "$OUT" Property | grep -q "^ALLUSERS"; then
  die "出现 ALLUSERS（不是 per-user 安装）"
fi
msiinfo export "$OUT" Directory | grep -qE "^INSTALLFOLDER[[:space:]]+ProgramsFolder" \
  || die "INSTALLFOLDER 未挂在 ProgramsFolder 下（每用户目录）"
msiinfo export "$OUT" Directory | grep -qE "^ProgramsFolder[[:space:]]+LocalAppDataFolder" \
  || die "ProgramsFolder 未挂在 LocalAppDataFolder 下"
echo "  ✅ per-user：无 ALLUSERS，安装目录 = [LocalAppDataFolder]Programs\\LinkX"

echo "  --- 自定义动作校验（防火墙，类型 50 = ExeCommand + Property） ---"
msiinfo export "$OUT" CustomAction | awk -F'\t' '$1=="FwAdd"||$1=="FwDel"{print "  "$1" type="$2" source="$3}'
for a in FwAdd FwDel; do
  msiinfo export "$OUT" CustomAction | awk -F'\t' -v a="$a" '$1==a{found=1} END{exit !found}' \
    || die "CustomAction 缺 $a"
done
msiinfo export "$OUT" CustomAction | grep -q 'advfirewall firewall add rule name="LinkX"' \
  || die "FwAdd 命令行不正确"
# 删除必须带过滤条件：光 `name="LinkX"` 会把机器上所有同名规则一起删（netsh 的语义）
msiinfo export "$OUT" CustomAction | grep -q 'delete rule name="LinkX" protocol=TCP localport=55676 dir=in' \
  || die "FwDel 命令行不正确（缺 protocol/localport/dir 过滤，会误删他人同名规则）"
msiinfo export "$OUT" Property | grep -q '^NETSH_EXE.*netsh\.exe' \
  || die "缺 NETSH_EXE 属性（类型 50 需指向 netsh.exe 全路径）"

echo "  --- 卸载清残留校验 ---"
for rf in RemoveInstallFolder RemoveProgramMenuDir; do
  msiinfo export "$OUT" RemoveFile | grep -q "$rf" || die "RemoveFile 缺 $rf"
done
echo "  ✅ RemoveFile（安装目录 + 开始菜单目录）/ 组件内 Registry 由标准动作自动清除"

echo "  --- 回环解包（文件须与源逐字节一致） ---"
TMP="$(mktemp -d)"
msiextract -C "$TMP" "$OUT" >/dev/null
# msiextract 以 LocalAppDataFolder 为根展开 → 实际落在 <TMP>/Programs/LinkX/linkx.exe
EXTRACTED="$(find "$TMP" -name linkx.exe -print -quit)"
[ -n "$EXTRACTED" ] || die "回环解包未找到 linkx.exe"
cmp "$EXTRACTED" "$EXE" || die "回环解包的 linkx.exe 与源不一致"
echo "  ✅ linkx.exe 回环一致（$(stat -c%s "$EXTRACTED") B；解包路径 ${EXTRACTED#"$TMP"/}）"
rm -rf "$TMP"
fi

echo
echo "== 6) 归档 $OUT_DIR/ + SHA-256 =="
mkdir -p "$OUT_DIR"
cp -f "$OUT" "$OUT_DIR/LinkX-$VERSION-x64.msi"
( cd "$OUT_DIR" && sha256sum "LinkX-$VERSION-x64.msi" > "LinkX-$VERSION-x64.msi.sha256" )
ls -lh "$OUT_DIR/LinkX-$VERSION-x64.msi"
cat "$OUT_DIR/LinkX-$VERSION-x64.msi.sha256"
echo "✅ Windows MSI 交付完成（未签名；SmartScreen 需「更多信息 → 仍要运行」）"

# 装机前先问一句"这台机器上已经有几份 LinkX"：同一 ProductCode 落在 per-machine 与
# per-user 两个作用域时，msiexec 返回 0 却留下两份程序、两套身份（对端因此变成"新设备"）。
# 这里只提示不阻断——出包成功与否不该被本机状态决定，但装之前必须看见它。
echo
echo "== 7) 本机安装份数核对 =="
if bash "$ROOT/Scripts/check-install-single.sh"; then
  echo "  （装完这份 MSI 后建议再跑一次：bash Scripts/check-install-single.sh）"
else
  echo "  ⚠️  先按上面的说明清掉多余那份再装，否则你会拿到两套身份、又要重新配对。"
fi