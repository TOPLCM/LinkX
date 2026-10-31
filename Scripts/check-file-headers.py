"""公开树门禁：每个源文件开头都要有一句"这是什么"。

为什么这道门禁在公开仓的发布路径上：公开仓的读者是**从某一个文件进来看的** ——
搜索结果、点目录树、点报错堆栈，不是先读完 README 才开始读代码。
文件第一屏没有说明，他看到的就是一堆 import 和函数名，只能自己猜；
而"猜"这件事的成本，正是开源项目最该替读者省掉的。

判据按文件类型认不同的开头标记：Rust 的 `//!`、Kotlin 的 KDoc、脚本的 `#` 注释、
proto 的 `//`、wxs 的 XML 注释块（XML 声明与 shebang 允许在前面）。
认不出就是没写 —— 判失败的报出文件名，一眼能补。

用法：
    python Scripts/check-file-headers.py [目录…]
不带参数查全仓（会跳过工具链、产物与内部目录）；带目录只查那一处的树。
"""

import io
import os
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

# 与 Scripts/export-public.sh 的排除口径一致：内部材料不进公开仓，也就不必有对外说明
INTERNAL_DIRS = ('Web', 'Skills', '.local-ide', '内部文档', '内部归档')
INTERNAL_FILES = ('.local-ide-ignore', 'Scripts/export-public.sh', 'Scripts/git-snapshot.sh')
INTERNAL_PATHS = ('Docs/README.md',)
SKIP_DIRS = {'.git', 'Target', 'Release', 'Temp', 'Tools', 'node_modules',
              'build', 'gradle', '.cargo'}
MARKS = {'.rs': ('//!', '//'), '.kt': ('/**', '//'), '.sh': ('#',), '.py': ('#', '"""'),
         '.ps1': ('#',), '.proto': ('//',), '.wxs': ('<!--',)}


def excluded(rel):
    for d in INTERNAL_DIRS:
        if rel == d or rel.startswith(d + '/'):
            return True
    for f in INTERNAL_FILES:
        if rel == f:
            return True
    return rel in INTERNAL_PATHS


def has_header(path, ext):
    try:
        lines = io.open(path, encoding='utf-8', errors='replace').read().split('\n')
    except OSError:
        return True, 0
    for i, line in enumerate(lines[:12]):
        s = line.strip()
        if not s:
            continue
        if s.startswith('#!') or s.startswith('<?xml'):
            continue
        if ext == '.rs' and s.startswith('#!['):
            continue
        return any(s.startswith(m) for m in MARKS[ext]), i + 1
    return False, 0


def scan(base):
    bad, total = [], 0
    for root, dirs, files in os.walk(base):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for f in files:
            ext = os.path.splitext(f)[1]
            if ext not in MARKS:
                continue
            rel = os.path.relpath(os.path.join(root, f), base).replace('\\', '/')
            if excluded(rel):
                continue
            total += 1
            ok, _ = has_header(os.path.join(root, f), ext)
            if not ok:
                bad.append(rel)
    return total, bad


def main():
    scopes = sys.argv[1:] or [os.path.dirname(os.path.dirname(os.path.abspath(__file__)))]
    allbad = []
    total = 0
    for s in scopes:
        t, bad = scan(s)
        total += t
        allbad += [(s, b) for b in bad]
    if allbad:
        print('✗ %d 个源文件开头没有"这是什么"的说明：' % len(allbad))
        for s, b in allbad[:30]:
            print('    %s' % b)
        sys.exit(1)
    print('✓ %d 个源文件开头都有"这是什么"的说明' % total)


main()
