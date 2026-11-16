#!/usr/bin/env bash
# 本机布局的单一注入口。此文件不入库，变量未配置时全为空，相应候选自动跳过。
#
# 工具链与密钥位置通过环境变量或本文件提供，仓库内脚本不写死本机路径。
#
# 可选变量（都用绝对路径或相对仓库根的路径）：
#   LINKX_TOOLS       本机工具链副本的根（里面按 Toolchain/Gradle、Toolchain/Android-SDK、Wix 布局）
#   LINKX_KEYSTORE    测试签名密钥位置；不设则落在仓库内的 .local-keys/
set -u

# 工具链候选根：没配就是空，调用方按"候选不存在则跳过"处理
LINKX_TOOLCHAIN_ROOT="${LINKX_TOOLS:-}"
# 测试密钥默认位置改成仓库内的 .local-keys/（同样不入库），不再指向任何私有目录名
LINKX_TEST_KEYSTORE_DEFAULT="$ROOT/.local-keys/linkx-test.jks"
