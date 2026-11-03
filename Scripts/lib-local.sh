#!/usr/bin/env bash
# 本机布局的单一注入口。**这个文件不入库**（内部仓与公开仓都被 .gitignore 排除），
# 所以仓库里任何脚本都不需要写出维护者本机的目录名——外人克隆下来这文件不存在，
# 下面这些变量全为空，相应候选自动跳过；维护者本机写了什么，只有他自己知道。
#
# 为什么要有这一层：0.5.3 之前，出包脚本把维护者本机的工具链目录名直接写在源码行里。
# 那些路径在别人的克隆里根本不存在，却把私有工作区的结构对外讲了一遍。
#
# 可选变量（都用绝对路径或相对仓库根的路径）：
#   LINKX_TOOLS       本机工具链副本的根（里面按 Toolchain/Gradle、Toolchain/Android-SDK、Wix 布局）
#   LINKX_KEYSTORE    测试签名密钥位置；不设则落在仓库内的 .local-keys/
set -u

_linkx_local_file="${LINKX_LOCAL_FILE:-$ROOT/.linkx-local}"
if [ -f "$_linkx_local_file" ]; then
  # shellcheck disable=SC1090
  . "$_linkx_local_file"
fi

# 工具链候选根：没配就是空，调用方按"候选不存在则跳过"处理
LINKX_TOOLCHAIN_ROOT="${LINKX_TOOLS:-}"
# 测试密钥默认位置改成仓库内的 .local-keys/（同样不入库），不再指向任何私有目录名
LINKX_TEST_KEYSTORE_DEFAULT="$ROOT/.local-keys/linkx-test.jks"
