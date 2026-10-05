import hashlib
import io
import re
import sys

# 控制台是 GBK 时，print('  ✓ …') 会抛 UnicodeEncodeError 把门禁本身搞崩——
# 崩掉的门禁读起来像"失败"，而它其实什么都没测。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

rows = re.findall(
    r'\|\s*(Windows|Android)\s*\|\s*`(LinkX-[^`]+)`[^|]*\|\s*([\d,]+)\s*\|\s*`([0-9a-f]{64})`',
    io.open('Release/README.md', encoding='utf-8').read(),
)
if len(rows) != 3:
    sys.exit('只解析到 %d 行（应为 3）——表格格式变了，这个检查本身失效了' % len(rows))

# 目录从表格里的平台列推，不写死文件名：写死版本号的话，下一次换版本就是 KeyError，
# 而"台账脚本崩了"很容易被读成"产物有问题"。
base = {'Windows': 'Release/Windows/', 'Android': 'Release/Android/'}
bad = 0
for plat, name, size, digest in rows:
    path = base[plat] + name
    raw = open(path, 'rb').read()
    real = hashlib.sha256(raw).hexdigest()
    ok = real == digest and len(raw) == int(size.replace(',', ''))
    print(('  ✓ ' if ok else '  ✗ ') + '%-28s %10d B' % (name, len(raw)))
    if not ok:
        print('     登记 %s / %s\n     盘上 %s / %d' % (digest[:16], size, real[:16], len(raw)))
        bad += 1
print('台账与盘上一致' if bad == 0 else '有 %d 条不一致' % bad)
sys.exit(1 if bad else 0)
