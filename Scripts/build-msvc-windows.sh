#!/usr/bin/env bash
# 编译 Windows 壳的**原生 ARM64** 交付产物（aarch64-pc-windows-msvc）。
# 用法：bash Scripts/build-msvc-windows.sh
# 产物：Target/aarch64-pc-windows-msvc/release/linkx.exe
#
# 为什么不并进 build-mingw-windows.sh 加个参数：那条是"任何宿主都能交叉编"的 MinGW 路线，
# 这条只能在 Windows 宿主上跑 —— 要 MSVC 的 link.exe 与 Windows SDK 的 ARM64 库
# （MinGW-w64 没有 aarch64-w64-mingw32 这条线）。两条混一个脚本，报错会完全对不上缺的东西。
#
# 为什么值得出原生包：x64 包在 ARM64 电脑上靠转译层跑，那层吃内存也吃性能，且已有高通笔记本
# 用户反馈功能对不上。**本机没有 ARM64 硬件**，所以这里只能验到"产物形态 + 安装包表级判据"，
# 运行期结论仍是"未实测" —— 台账与 Release 必须这么写。完整背景见 Docs/Build.md。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TRIPLE="aarch64-pc-windows-msvc"
EXE="Target/$TRIPLE/release/linkx.exe"

command -v cargo >/dev/null || { echo "❌ 找不到 cargo"; exit 1; }
rustup target add "$TRIPLE" >/dev/null 2>&1 || true

echo "== 原生 ARM64 Windows 壳（$TRIPLE）=="
if ! cargo build --release --target "$TRIPLE" --manifest-path Platforms/Windows/Cargo.toml; then
  echo "❌ 编译失败。最常见的原因是缺 MSVC 的 ARM64 交叉工具链或 Windows SDK 的 ARM64 库："
  echo "   vs_buildtools --quiet --wait --norestart --installPath <目录> \\"
  echo "     --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 \\"
  echo "     --add Microsoft.VisualStudio.Component.Windows11SDK.22621"
  echo "   图标资源这一步需要 SDK 的 rc.exe（build.rs 会自己找，也可用 LINKX_RC=<全路径> 指定）"
  exit 1
fi
[ -f "$EXE" ] || { echo "❌ 未产出 $EXE"; exit 1; }

# 字节级断言 PE 机器码：ARM64 = 0xAA64。"目标是 ARM64"不等于"产物是 ARM64"——
# 链接器指错、缓存了旧产物、或者工具链静默退回到 x64，表现都是一个"名字对、机器码不对"的包。
PY="$(command -v python3 || command -v python)"
[ -n "$PY" ] || { echo "❌ 找不到 python，无法核对 PE 机器码"; exit 1; }
MACHINE="$("$PY" - "$EXE" << 'PYEOF'
import struct, sys
d = open(sys.argv[1], 'rb').read(4096)
pe = struct.unpack_from('<I', d, 0x3C)[0]
print('0x%04X' % struct.unpack_from('<H', d, pe + 4)[0])
PYEOF
)"
echo "  PE Machine = $MACHINE"
[ "$MACHINE" = "0xAA64" ] || { echo "❌ 期望 0xAA64（ARM64），实际 $MACHINE —— 这份不是 ARM64 产物"; exit 1; }

# 交付构建卫生：图标资源必须真的在（build.rs 找不到 rc.exe 会直接失败，这里是第二道）。
# .rsrc 段缺失的 exe 装得上也能跑，只是资源管理器、任务栏、卸载面板全是默认图标。
HAS_RSRC="$("$PY" - "$EXE" << 'PYEOF'
import struct, sys
# 控制台是 GBK 时打印 ✓/❌ 会抛 UnicodeEncodeError —— 崩在这里会被读成"产物有问题"，
# 而它其实什么都没判（本仓第 5 条坑）。
sys.stdout.reconfigure(encoding='utf-8', errors='replace')
d = open(sys.argv[1], 'rb').read()
pe = struct.unpack_from('<I', d, 0x3C)[0]
nsec = struct.unpack_from('<H', d, pe + 6)[0]
opt = struct.unpack_from('<H', d, pe + 20)[0]
tabs = pe + 24 + opt
found = False
for i in range(nsec):
    off = tabs + i * 40
    if d[off:off + 6].rstrip(b'\0') == b'.rsrc':
        found = struct.unpack_from('<I', d, off + 16)[0] > 0
print('yes' if found else 'no')
PYEOF
)"
[ "$HAS_RSRC" = "yes" ] || { echo "❌ exe 里没有 .rsrc 段（图标资源没编进去）—— 检查 rc.exe 与 linkx.rc"; exit 1; }

SIZE="$(wc -c < "$EXE" | tr -d ' ')"
echo "✅ ARM64 Windows 壳：$EXE（$SIZE 字节，.rsrc 在位）"

# 第三道断言：**不许依赖 VC++ 运行库**。
# MSVC 默认动态链 VCRUNTIME140.dll，而它是 VC++ 可再发行组件、不是 Windows 自带 ——
# 没装 ARM64 运行库的骁龙本上 exe 直接起不来。这台机器上编得出来、也跑得不了（我们是 x64 宿主），
# 所以只能在产物字节上拦：解析 PE 的导入目录，出现 VCRUNTIME*/MSVCP*/ucrtbase.dll 就是配置被谁改回了动态链。
"$PY" - "$EXE" << 'PYEOF'
import struct, sys
# 控制台是 GBK 时打印 ✓/❌ 会抛 UnicodeEncodeError —— 崩在这里会被读成"产物有问题"，
# 而它其实什么都没判（本仓第 5 条坑）。
sys.stdout.reconfigure(encoding='utf-8', errors='replace')
d = open(sys.argv[1], 'rb').read()
pe = struct.unpack_from('<I', d, 0x3C)[0]
magic = struct.unpack_from('<H', d, pe + 24)[0]
nsec = struct.unpack_from('<H', d, pe + 6)[0]
opt = struct.unpack_from('<H', d, pe + 20)[0]
tabs = pe + 24 + opt
# DataDirectory[1] = Import Table：PE32+ 偏移 pe+24+112，PE32 是 +96
dd = pe + 24 + (112 if magic == 0x20B else 96)
imp_rva, imp_size = struct.unpack_from('<II', d, dd + 8)
if imp_size == 0:
    print('  （没有导入表：全静态的 exe）')
    sys.exit(0)
secs = []
for i in range(nsec):
    o = tabs + i * 40
    # IMAGE_SECTION_HEADER：+8 VirtualSize / +12 VirtualAddress / +16 SizeOfRawData / +20 PointerToRawData
    vs, va, raw, ptr = struct.unpack_from('<IIII', d, o + 8)
    secs.append((va, vs, raw, ptr))
def r2o(rva):
    for va, vs, raw, ptr in secs:
        if va <= rva < va + max(vs, raw):
            return ptr + (rva - va)
    raise SystemExit('❌ PE 导入表 RVA 越界，这份 exe 读不懂，不敢放行')
def cstr(off):
    end = d.index(b'\0', off)
    return d[off:end].decode('ascii', 'replace')
dlls = []
for i in range(64):                      # 描述符每个 20 字节，读到全零为止
    off = r2o(imp_rva) + i * 20
    # IMAGE_IMPORT_DESCRIPTOR：+0 OriginalFirstThunk / +12 Name / +16 FirstThunk
    orig_first, name_rva, first_thunk = (struct.unpack_from('<I', d, off)[0],
                                         struct.unpack_from('<I', d, off + 12)[0],
                                         struct.unpack_from('<I', d, off + 16)[0])
    if name_rva == 0 and first_thunk == 0 and orig_first == 0:
        break
    dlls.append(cstr(r2o(name_rva)).lower())
# 只拦 VC++ 可再发行组件那一组：api-ms-win-crt-* 与 ucrtbase.dll 是 Win10+ 系统自带的 UCRT
    # （现在交付的 x64 包也依赖它们），把它们算进来会把好产物判成坏的。
    bad = [x for x in dlls if x.startswith(('vcruntime', 'msvcp', 'concrt', 'vccorlib'))]
print('  依赖的 DLL 数 = %d' % len(dlls))
if bad:
    print('❌ 依赖了 VC++/UCRT 运行库（目标机上未必有）：' + ', '.join(sorted(bad)))
    print('   需要 .cargo/config.toml 里 [target.aarch64-pc-windows-msvc] 的 +crt-static')
    sys.exit(1)
print('  ✓ 只依赖操作系统自带组件（无 VC++ 运行库依赖）')
PYEOF
