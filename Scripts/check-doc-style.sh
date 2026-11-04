#!/usr/bin/env bash
# 对外文档的语言门禁：禁词表来自《文档规范》第三节，改规则先改那份，再改这里。
#
# 这个脚本只做一件事——检查对外文档的措辞。私有工作区路径的检查不在这里：
# 那份匹配串本身就写着私有目录名，放进这个会发布的脚本等于换个地方泄露；
# 而且发布侧的导出自检已经在扫同样的东西，那里才有本机清单。
#
# 用法：bash Scripts/check-doc-style.sh
set -uo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

OUTWARD=(README.md DIRECTORY.md CHANGELOG.md THIRD_PARTY_LICENSES.md
         Docs/Build.md Docs/Security.md Docs/Limitations.md Docs/Debug-Plane.md
         Release/README.md)
NOTES=$(find Release/Notes -name '*.md' 2>/dev/null)

# 拟人比喻、口头禅、无判据夸张、第一人称。「静默」不算禁词：它是标准技术词
# （静默丢弃、静默失败），只要同句写清了行为就成立。
BANNED='悄悄|默默|顺手|闷头|干等|白送|吃内存|吃爆|顶穿|顶掉|挤掉|拖大|其实|反正|说白了|说句|话虽如此|值得一提的是|换句话说|简单说|彻底解决|根治|完美|一劳永逸|大概率|看起来正常|应该没问题|我自己|我这次|我上一轮|拆穿|打脸|翻车|我踩|教训是'

fail=0
echo "== 文档语言门禁 =="
for f in "${OUTWARD[@]}" $NOTES; do
  [ -f "$f" ] || continue
  hits=$(grep -oE "$BANNED" "$f" 2>/dev/null | sort | uniq -c | awk '{printf "%s×%s ", $2,$1}')
  if [ -n "$hits" ]; then echo "  ✗ $f 口语化：$hits"; fail=1; fi
done
[ $fail -eq 0 ] && echo "  ✓ 对外文档无口语化禁词（含 Release/Notes $(echo "$NOTES" | grep -c .) 篇）"

# 目录卡片必须是短指针；写成第二份文档就是分层跑偏。
# 只统计本仓里真实存在的 README，索引与台账类天然不在这条判据的范围内。
long=$(find . -name README.md -not -path "./.git/*" -not -path "./Docs/*" -not -path "./Release/*" \
        -not -path "./Target/*" -not -path "./.scratch/*" -not -path "./Temp/*" -not -path "./Tools/*" \
        -not -path "./Demo/*" -not -path "./Web/*" 2>/dev/null |
        while read -r f; do
          [ "$f" = "./README.md" ] && continue
          n=$(wc -l < "$f"); [ "$n" -gt 8 ] && echo "      $f（$n 行）"
        done)
if [ -n "$long" ]; then
  echo "  ℹ 以下目录卡片超出模板 F 的篇幅，检查是否在重复 DIRECTORY.md："
  echo "$long"
fi

exit $fail
