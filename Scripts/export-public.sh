#!/usr/bin/env bash
# 导出可公开发布的副本：取本仓库**已跟踪**的工作树文件，剔除工具链/产物/内部材料。
# 用法：bash Scripts/export-public.sh [目标目录]      默认 ../LinkX for Github
# 为什么用 git ls-files 而不是 git archive：要拿"当前工作树"（含未提交改动），
# 而 archive 只能拿某个 commit，容易导出一个和文档不同步的版本。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DEST="${1:-$ROOT/../LinkX for Github}"
# 自检要跳过副本里的本脚本自身：它的检查模式与排除规则里天然含有 HASEE / SuperLink-Lab /
# PRIVATE KEY 这些串，不跳过的话检查器会把自己当成泄露源。
SELF_SKIP='(\.export-public\.stamp$|/Scripts/export-public\.sh$)'

# 排除规则（正则，匹配 git ls-files 输出的相对路径）
#  1) 交付二进制：产物走 Releases 页面分发，源码树只留校验值与说明
#  2) 内部过程材料：本机环境交接、走查证据截图、审计归档、逐日会话日志
#  3) 维护者工具约定：IDE Skills、另一个工具的 ignore 配置
#  4) svg/ 已确认口径（iconfont.cn 下载、可公开免费使用），随仓库公开，见 THIRD_PARTY_LICENSES.md 三
#  5) Web/ 是官网那条线的素材，与产品源码是两回事；里面有收款码与作者照片，
#     按"收款码不进公开仓库"的既有口径整体排除（历史里那份也不该跟着发布）。
#  6) Docs/Spec/ 是立项与逐版修订记录（1880 行），里面引用了 8 处**不随仓库发布**的内部台账，
#     公开出去等于给读者一堆死链；对外需要的事实由 CHANGELOG / Security / Build / Limitations 承担。
#  7) Docs/Architecture/ 整目录是 2026-09-29（0.4.0 期）的架构快照，通篇引用内部编号，
#     且结论已经过期（图里还写着"手机→电脑方向未结案"，而那条 0.5.0 就结案了）。
#     **发一份过期的"当前架构与完成度"比不发更坏** —— 它会被当成现状读。
EXCLUDE='^(Release/Archive/.*/.*\.(apk|idsig|msi)$|Web/|Docs/Spec/|Docs/Architecture/|Skills/|Docs/Handover/|Docs/Diag/|Docs/Audit/|Docs/Review/|Docs/Memory/|Docs/Plan/|\.zcodeignore$|Scripts/restore-toolchain\.sh|Scripts/git-snapshot\.sh|Scripts/wine-ui-review\.sh|Scripts/repro-bug047\.py)'

echo "== 1) 取跟踪文件清单 =="
LIST="$(mktemp)"; KEEP="$(mktemp)"
git -c core.quotepath=off ls-files > "$LIST"
total=$(wc -l < "$LIST")
grep -vE "$EXCLUDE" "$LIST" > "$KEEP" || true
kept=$(wc -l < "$KEEP")
# 排除正则写错或排除过头，最典型的表现就是清单空掉或腰斩，而脚本照样打印"复制完成"。
[ -s "$KEEP" ] || { echo "  ✗ 公开清单为空：排除规则把 $total 个文件全排掉了"; exit 1; }
if [ "$kept" -le $((total / 2)) ]; then
  echo "  ✗ 公开清单只剩 $kept/$total，疑似排除规则写错（正常应只排掉少数内部材料）"
  exit 1
fi
echo "  跟踪 $total 个 → 公开 $kept 个（排除 $((total - kept)) 个）"
rm -f "$LIST" "$KEEP"

echo
echo "== 2) 清理并重建目标目录 =="
if [ -e "$DEST" ]; then
  # 只清理由本脚本生成的副本：目标目录里必须有标记文件才敢动，否则拒绝
  [ -f "$DEST/.export-public.stamp" ] || { echo "❌ $DEST 存在但不是本脚本导出的副本，拒绝覆盖"; exit 1; }
  # 清掉 stamp 之外的一切。以前只删硬编码的那几个目录名，于是"仓库里已删除的文件"
  # 会永久留在公开副本里 —— 删了 = 没发，这个假象很危险。
  find "$DEST" -mindepth 1 -maxdepth 1 ! -name '.export-public.stamp' -exec rm -rf {} +
fi
mkdir -p "$DEST"
echo "export-public.sh 生成的公开副本标记（删除此文件即让脚本拒绝覆盖此目录）" > "$DEST/.export-public.stamp"

echo
echo "== 3) 按清单复制（tar 一次管道） =="
# 为什么不用 while + cp：695 个文件要 fork 近 1400 个进程，实测每次 3 分钟以上，
# 而这个脚本每次发布都要重跑。tar 走一次管道就够，--null 对带空格/中文的文件名安全。
LISTZ="$(mktemp)"; KEEPZ="$(mktemp)"
git ls-files -z > "$LISTZ"
grep -zEv "$EXCLUDE" < "$LISTZ" > "$KEEPZ" || true
if tar -C "$ROOT" --null -cf - -T "$KEEPZ" | tar -xf - -C "$DEST"; then
  echo "  复制完成"
else
  echo "❌ 复制失败：清单里有文件在工作树里不存在？先 git status 看一眼"; rm -f "$LISTZ" "$KEEPZ"; exit 1
fi
rm -f "$LISTZ" "$KEEPZ"

echo
echo "== 4) 公开前自检 =="
fail=0
# 硬失败：真的密钥文件（内容级），以及个人/竞品/机器私有路径
keys=$(find "$DEST" -type f \( -name '*.jks' -o -name '*.keystore' -o -name '*.p12' \
      -o -name 'id_rsa*' -o -name '*.pem' \) 2>/dev/null || true)
if [ -n "$keys" ]; then echo "  ✗ 副本里有密钥文件："; echo "$keys" | sed 's/^/      /'; fail=1
else echo "  ✓ 无密钥文件（*.jks/*.keystore/*.pem/id_rsa）"; fi
pemhits=$(grep -rlIE -- '-----BEGIN [A-Z ]*PRIVATE KEY-----' "$DEST" 2>/dev/null \
          | grep -vE "$SELF_SKIP" || true)
if [ -n "$pemhits" ]; then
  echo "  ✗ 出现私钥 PEM 标记，且不在已知良性清单里："; echo "$pemhits" | sed 's/^/      /'; fail=1
else
  echo "  ✓ 无私钥 PEM 标记"
fi
chk() { # chk <说明> <grep -E 模式>
  local hits
  hits="$(grep -rlIE "$2" "$DEST" 2>/dev/null | grep -vE "$SELF_SKIP" | head -5 || true)"
  if [ -n "$hits" ]; then echo "  ✗ $1"; echo "$hits" | sed 's/^/      /'; fail=1
  else echo "  ✓ $1"; fi
}
chk "无个人机器用户名/邮箱" 'C:\\+Users\\+HASEE|HASEE@local'
chk "无竞品逆向工程目录引用" 'SuperLink-Lab'
# 路径级检查：内部记忆/计划/维护者脚本一旦混进副本，内容级 grep 是查不出来的
leak=$(cd "$DEST" && find . \( -path './Docs/Memory/*' -o -path './Docs/Plan/*' -o -path './Web/*' \
       -o -name 'git-snapshot.sh' -o -name 'restore-toolchain.sh' \
       -o -name 'wine-ui-review.sh' -o -name 'repro-bug047.py' \) 2>/dev/null | head -5)
if [ -n "$leak" ]; then
  echo "  ✗ 内部材料混进公开副本："; echo "$leak" | sed 's/^/      /'; fail=1
else
  echo "  ✓ 内部记忆/计划/官网素材与维护者侧脚本未进副本"
fi
# 打赏图 / 作者真人照：只查**文件名与官网路径**，不查"打赏"这个词 —— 关于页的代码注释里
# 正经写着"外链与打赏都不上 UI"，那是实话，不是泄露（拿词当判据会把真话判成违规）。
assets=$(cd "$DEST" && find . \( -iname 'donate*' -o -iname 'author.*' -o -iname '*wechat*' \
         -o -path './Web/*' \) -type f 2>/dev/null | head -5)
if [ -n "$assets" ]; then
  echo "  ✗ 打赏图 / 作者真人照 / 官网素材混进副本："; echo "$assets" | sed 's/^/      /'; fail=1
else
  echo "  ✓ 无打赏图、无作者真人照、无官网素材"
fi
info() { echo "  ℹ $1：$(grep -rlIE "$2" "$DEST" 2>/dev/null | grep -vE "$SELF_SKIP" | wc -l) 个文件提及（历史/内部环境叙述，不判失败）"; }
info "维护者本机工具链写法（/root/.cargo、Tools/Toolchain、restore-toolchain.sh）" '/root/\.cargo|Tools/Toolchain|restore-toolchain'
[ -f "$DEST/LICENSE" ] && head -1 "$DEST/LICENSE" | grep -q "GNU GENERAL PUBLIC LICENSE" \
  && echo "  ✓ LICENSE 是 GPL-3.0 全文" || { echo "  ✗ LICENSE 缺失或不是 GPL-3.0"; fail=1; }
for f in README.md DIRECTORY.md THIRD_PARTY_LICENSES.md Docs/Debug-Plane.md \
         Licenses/localsend/NOTICE Licenses/localsend/LICENSE; do
  [ -f "$DEST/$f" ] && echo "  ✓ 必备文件在位：$f" || { echo "  ✗ 缺 $f"; fail=1; }
done

echo
echo "  ── 以下为人工复核项（不判失败，命中即列出看一眼）──"
echo "  · 仍在申请软著的说法（应为 0）：$(grep -rnIE '正在申请软著|软著申请进行中|需申请软著|软著要求仓库不公开' "$DEST" 2>/dev/null | grep -vE "$SELF_SKIP" | wc -l)"
grep -rnI -E '软著|软件著作权' "$DEST" 2>/dev/null | sed 's/^/      /' | head -12 || true
echo "  · 把 AGPL 当现行许可证的说法（人工确认语境是否定或历史）："
grep -rnI -E 'AGPL' "$DEST" 2>/dev/null | sed 's/^/      /' | head -12 || true

echo
echo "== 5) 结果 =="
du -sh "$DEST" 2>/dev/null | sed 's/^/  体积：/'
find "$DEST" -type f ! -name '.export-public.stamp' | wc -l | sed 's/^/  文件数：/'
[ "$fail" = 0 ] && echo "  ✅ 导出完成：$DEST" || echo "  ⚠ 导出完成但自检有失败项，公开前必须处理"
exit "$fail"
