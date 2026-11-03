#!/usr/bin/env bash
# 版本号单一真源门禁：真源是 workspace Cargo.toml，各落点必须一致。
#
# 版本号散在 Cargo.toml、Kotlin versionName、WXS、文档头部、写死字面量的 Rust 断言里。
# 漏掉任一处的表现不是"编译不过"，而是"某个包悄悄带着旧版本号发出去"——
# About 页、宣传材料、MSI 属性页互相矛盾。
#
# 用法：
#   bash Scripts/check-version-sync.sh                # 开发期检查（代码内一致）
#   bash Scripts/check-version-sync.sh --require-docs # 出包前追加检查文档基线
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
die() { echo "  ✗ $*" >&2; fail=1; }
ok()  { echo "  ✓ $*"; }

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || die "无法从 Cargo.toml 读取版本号（单一真源本身坏了）"
echo "== 版本同步门禁（真源 Cargo.toml = $VERSION）=="

# ---------- 1) Kotlin versionName ----------
KTS="Platforms/Android/app/build.gradle.kts"
KVER="$(sed -n 's/^[[:space:]]*versionName = "\([^"]*\)".*/\1/p' "$KTS" | head -1)"
KCODE="$(sed -n 's/^[[:space:]]*versionCode = \([0-9]*\).*/\1/p' "$KTS" | head -1)"
if [ "$KVER" = "$VERSION" ]; then
  ok "Android versionName = $KVER（versionCode=$KCODE）"
else
  die "Android versionName=$KVER 与 Cargo.toml=$VERSION 不一致"
fi
[ -n "$KCODE" ] || die "Android versionCode 读不到（必须是数字）"

# ---------- 2) README 首屏的版本号必须跟真源一致 ----------
# README 是外面的人第一眼读到的地方，写着旧版本号 = 一开口就是假话。
# 判据是"不许写错版本号"而非"必须写版本号"：首屏不提版本是合法选择，提了就必须在真源上。
if ! grep -q "当前版本" README.md; then
  ok "README 首屏不提版本号（不强制）"
elif grep -qE "当前版本 \*\*v?$VERSION\*\*" README.md; then
  ok "README 首屏版本 = $VERSION"
else
  die "README.md 写了「当前版本」但不是 $VERSION（改 README，别改门禁）"
fi

# ---------- 3) 安装器模板不得写死版本 ----------
for wxs in Platforms/Windows/installers/*.wxs; do
  if grep -qE 'Version="[0-9]+\.[0-9]+' "$wxs" && ! grep -q '@VERSION@' "$wxs"; then
    die "$wxs 里出现了硬编码版本号（应由 build-msi-windows.sh 注入 @VERSION@）"
  fi
done
ok "WXS 模板走 @VERSION@ 注入"

# ---------- 4) 版本断言不许写死字面量 ----------
# 写死后的断言改版本时最容易漏，而且漏了也照样全绿（它断言的是旧值）。
# 模式要同时吃掉 `== "0.x"` 和 `assert_eq!(X, "0.x")`：只写前者会放过最常见的 `assert_eq!`。
HARD="$(grep -rnE 'LINKX_FFI_VERSION[ ,=!<>]*"0\.|version\(\), *"0\.|== "0\.[0-9]+\.[0-9]+"' \
        --include=*.rs Crates Platforms 2>/dev/null | grep -v CARGO_PKG_VERSION || true)"
if [ -n "$HARD" ]; then
  die "Rust 里仍有写死的版本断言：\n$HARD"
else
  ok "版本断言一律取 env!(\"CARGO_PKG_VERSION\")"
fi

# ---------- 5) 出包前才查文档基线 ----------
# 基线文件路径由 DOC_BASE 给出（维护者本机在 .linkx-local 里配自己的内部基线）；
# 没配就直接查 CHANGELOG——那才是读者能看到的东西，缺文件不等于跳过检查。
if [ "${1:-}" = "--require-docs" ]; then
  DOC_BASE="${DOC_BASE:-CHANGELOG.md}"
  if [ -f "$DOC_BASE" ] && [ "$DOC_BASE" != "CHANGELOG.md" ]; then
    DOCVER="$(sed -n 's/^- \*\*\([0-9][0-9]*\.[0-9]*\.[0-9]*\)\*\*.*/\1/p' "$DOC_BASE" | head -1)"
    [ -n "$DOCVER" ] || die "$DOC_BASE 里读不到形如「- **x.y.z**」的版本行（基线格式变了）"
  else
    DOC_BASE=CHANGELOG.md
    DOCVER="$(sed -n 's/^## \([0-9][0-9]*\.[0-9]*\.[0-9]*\).*/\1/p' "$DOC_BASE" | head -1)"
    [ -n "$DOCVER" ] || die "没在 $DOC_BASE 读到「## x.y.z」这样的版本行"
  fi
  if [ "$DOCVER" = "$VERSION" ]; then
    ok "$DOC_BASE 当前交付版本 = $DOCVER"
  else
    die "$DOC_BASE 当前交付版本=$DOCVER 与 Cargo.toml=$VERSION 不一致（出包前必须对齐）"
  fi
  # 交付集就是这三个：x64 与 ARM64 两个电脑端包 + 一个双 ABI 手机端包。
  # 少一个就当场报，别等到 Releases 页上才发现"这版怎么没有 ARM64 包"。
  for f in Release/Windows/LinkX-$VERSION-x64.msi Release/Windows/LinkX-$VERSION-arm64.msi \
           Release/Android/LinkX-$VERSION-release.apk; do
    [ -f "$f" ] || die "缺少交付产物 $f"
  done
fi

[ "$fail" = 0 ] && { echo "全部通过"; exit 0; } || { echo "版本同步门禁未通过"; exit 1; }
