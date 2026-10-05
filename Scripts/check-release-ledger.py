import hashlib
import io
import os
import re
import sys

# 控制台是 GBK 时，print('  ✓ …') 会抛 UnicodeEncodeError 把门禁本身搞崩——
# 崩掉的门禁读起来像"失败"，而它其实什么都没测。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

text = io.open('Release/README.md', encoding='utf-8').read()
rows = re.findall(
    r'\|\s*(Windows|Android)\s*\|\s*`(LinkX-[^`]+)`[^|]*\|\s*([\d,]+)\s*\|\s*`([0-9a-f]{64})`',
    text,
)
# 行数不写死（某一版可以少一个产物，比如不发调试包），但一行都没有就是表格格式变了、
# 这道检查已经什么都不测了。登记与盘上必须互相对得上，见下面的双向比对。
if not rows:
    sys.exit('一行都没解析到——表格格式变了，这个检查本身失效了')

# 目录从表格里的平台列推，不写死文件名：写死版本号的话，下一次换版本就是 KeyError，
# 而"台账脚本崩了"很容易被读成"产物有问题"。
base = {'Windows': 'Release/Windows/', 'Android': 'Release/Android/'}
bad = 0

listed = {plat: set() for plat in base}
for plat, name, _size, _digest in rows:
    listed[plat].add(name)
for plat, d in base.items():
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
print('台账与盘上一致' if bad == 0 else '有 %d 条不一致' % bad)
sys.exit(1 if bad else 0)
