"""发布台账核对：`Release/README.md` 里登记的校验值与盘上的产物必须一致。

为什么要这道门禁：台账里的 SHA-256 是用户下载后核对"拿到的是不是同一个文件"的唯一依据。
台账写错、或者盘上的包与台账不是同一版，比没有台账更糟 —— 用户会照着一个错的值去判断，
而他手里的包其实没问题。台账与盘上**双向**比对：登记了盘上没有的、盘上有没登记的，都要报。

用法：python Scripts/check-release-ledger.py
"""

import hashlib
import io
import os
import re
import sys

# 控制台是 GBK 时，print('  ✓ …') 会抛 UnicodeEncodeError 把门禁本身搞崩——
# 崩掉的门禁读起来像"失败"，而它其实什么都没测。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

text = io.open('Release/README.md', encoding='utf-8').read()
# 只解析「当前版本」那一节的表：历版校验值表（`## 历版校验值`）用的是同样的列格式，
# 一起抓会把 25 条历史登记当成"盘上没有的产物"全部误报，而真正该比对的那几条反而被淹没。
cur_section = text.split('## 历版校验值')[0]
rows = re.findall(
    r'\|\s*(Windows|Android)\s*\|\s*`(LinkX-[^`]+)`[^|]*\|\s*([\d,]+)\s*\|\s*`([0-9a-f]{64})`',
    cur_section,
)
# 行数不写死（某一版可以少一个产物，比如不发调试包），但一行都没有就是表格格式变了、
# 这道检查已经什么都不测了。登记与盘上必须互相对得上，见下面的双向比对。
if not rows:
    sys.exit('一行都没解析到——表格格式变了，这个检查本身失效了')
# 上面按「## 历版校验值」切过一刀，所以要再确认当前版的三条真在里面：
# 否则一旦有人把它们挪到那一节之后，这里就会解析到历版行或零行，而门禁照样打印"一致"。
cur_ver = os.environ.get('LINKX_CUR_VER') or ''
if cur_ver and not any(cur_ver in name for _p, name, _s, _d in rows):
    sys.exit('当前版本 %s 的产物没出现在待核对行里——表格分节把当前版表划到历版之后了' % cur_ver)

# 目录从表格里的平台列推，不写死文件名：写死版本号的话，下一次换版本就是 KeyError，
# 而"台账脚本崩了"很容易被读成"产物有问题"。
base = {'Windows': 'Release/Windows/', 'Android': 'Release/Android/'}
bad = 0

listed = {plat: set() for plat in base}
for plat, name, _size, _digest in rows:
    listed[plat].add(name)
skipped = []
for plat, d in base.items():
    if not os.path.isdir(d):
        # 克隆里没有二进制（`Release/Windows/*`、`Release/Android/*` 都不入库），盘上比对无从做起。
        # 报"跳过"而不是抛 FileNotFoundError：崩掉的门禁读起来像"产物坏了"，而它什么都没比。
        skipped.append(plat)
        continue
    on_disk = {f for f in os.listdir(d) if not f.endswith(('.sha256', '.idsig'))}
    for label, diff in (('盘上有未登记的产物', on_disk - listed[plat]),
                        ('登记了盘上不存在的文件', listed[plat] - on_disk)):
        if diff:
            print('  ✗ %s %s：%s' % (plat, label, '、'.join(sorted(diff))))
            bad += 1

for plat, name, size, digest in rows:
    path = base[plat] + name
    if not os.path.exists(path):
        continue  # 上一条已经报过"登记了盘上不存在的文件"
    raw = open(path, 'rb').read()
    real = hashlib.sha256(raw).hexdigest()
    ok = real == digest and len(raw) == int(size.replace(',', ''))
    print(('  ✓ ' if ok else '  ✗ ') + '%-28s %10d B' % (name, len(raw)))
    if not ok:
        print('     登记 %s / %s\n     盘上 %s / %d' % (digest[:16], size, real[:16], len(raw)))
        bad += 1
if skipped:
    print('  ⓘ 本机没有这些产物目录，盘上比对跳过：%s（这一项在出包机上跑）' % '、'.join(skipped))
if bad:
    print('有 %d 条不一致' % bad)
elif len(skipped) == len(base):
    # 什么都没比：只核了表格自身能解析，别说成"一致"
    print('只核了台账表格本身（本机没有产物，未比对盘上文件）')
else:
    print('台账与盘上一致')
sys.exit(1 if bad else 0)
