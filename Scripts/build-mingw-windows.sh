#!/usr/bin/env bash
# Linux 交叉编译 Windows 壳，交付目标是 x86_64-pc-windows-gnu（不是 msvc）。
# 为什么 x64 用 gnu：这条链任何宿主都能交叉编（Linux/Windows 都行），CI 的 exe 就是在这里出的；
# 图标资源由 MinGW 的 windres 编。ARM64 原生包是另一条链（MSVC + SDK，只能在 Windows 宿主上跑），
# 见 Scripts/build-msvc-windows.sh —— 两份产物各自带图标，build.rs 两条都要求资源真的编进去。
# 用法：bash Scripts/build-mingw-windows.sh
# 产物：Target/x86_64-pc-windows-gnu/release/linkx.exe
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1 || true
cargo build --release --target x86_64-pc-windows-gnu --manifest-path Platforms/Windows/Cargo.toml
EXE="Target/x86_64-pc-windows-gnu/release/linkx.exe"
file "$EXE" | grep -q "PE32+" && echo "✅ Windows shell 交叉编译产物: $EXE"