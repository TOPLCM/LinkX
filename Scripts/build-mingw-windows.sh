#!/usr/bin/env bash
# Linux 交叉编译 Windows 壳，交付目标是 x86_64-pc-windows-gnu（不是 msvc）。
# 为什么是 gnu：build.rs 只在 windows-gnu 下调用 windres 编图标资源；msvc 目标跳过这一步，
# 产出的 exe 在资源管理器、任务栏、卸载面板里全是默认图标。
# 用法：bash Scripts/build-mingw-windows.sh
# 产物：Target/x86_64-pc-windows-gnu/release/linkx.exe
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1 || true
cargo build --release --target x86_64-pc-windows-gnu --manifest-path Platforms/Windows/Cargo.toml
EXE="Target/x86_64-pc-windows-gnu/release/linkx.exe"
file "$EXE" | grep -q "PE32+" && echo "✅ Windows shell 交叉编译产物: $EXE"