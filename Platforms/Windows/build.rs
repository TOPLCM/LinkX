//! 把 `LinkX.ico`（资源 id 1）编进 `linkx.exe`，使 exe 本身带图标
//! （资源管理器 / 任务栏 / 开始菜单 / 卸载面板）；MSI 侧另由 `LinkX.wxs` 的 Icon 表提供。
//!
//! 注意：本 crate 也会被非 Windows 目标编译（工作区统一 `cargo check`），
//! 故非 Windows 目标直接跳过，不打扰 Linux/Android 构建。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=linkx.rc");
    println!("cargo:rerun-if-changed=LinkX.ico");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let windres = match target.as_str() {
        "x86_64-pc-windows-gnu" => "x86_64-w64-mingw32-windres",
        "i686-pc-windows-gnu" => "i686-w64-mingw32-windres",
        _ => {
            println!("cargo:warning=未适配的 Windows 目标 {target}，跳过图标资源编译");
            return;
        }
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    let res = out_dir.join("linkx.res");
    let status = Command::new(windres)
        .args(["-O", "coff", "-I", ".", "-i", "linkx.rc", "-o"])
        .arg(&res)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg={}", res.display()),
        Ok(s) => panic!("windres 编译 linkx.rc 失败，退出码 {s}"),
        Err(e) => panic!("无法执行 {windres}：{e}（Windows 交叉编译需 MinGW 工具链）"),
    }
}
