//! 把 `LinkX.ico`（资源 id 1）编进 `linkx.exe`，使 exe 本身带图标
//! （资源管理器 / 任务栏 / 开始菜单 / 卸载面板）；MSI 侧另由 `LinkX.wxs` 的 Icon 表提供。
//!
//! 两条资源链，按目标分开：
//! - `*-windows-gnu`（交付用的 x64 包）：MinGW 的 `windres`；
//! - `*-windows-msvc`（ARM64 原生包）：Windows SDK 的 `rc.exe`。
//!
//! **两个 Windows 目标都要求资源真的编进去**：早先 msvc 是直接 return 跳过的，
//! 产物能跑但 exe 没图标（资源管理器、任务栏、卸载面板全是默认图标），而 CI 不会拦这种"少个图标"。
//! 现在找不到 `rc.exe` 就 panic —— 宁可可看见地失败，不要静默降格。
//!
//! 非 Windows 目标（工作区统一 `cargo check`）仍然直接跳过，不打扰 Linux/Android 构建。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 在 Windows SDK 的常见落点里找 `rc.exe`。
///
/// 不假设安装在默认盘：`vs_buildtools.exe --installPath` 可以把整个工具链装到别的目录，
/// 但 Windows SDK 仍然会写到机器级的 `Windows Kits\10`，所以两边都要找。
fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LINKX_RC") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("WindowsSdkDir") {
        roots.push(PathBuf::from(dir).join("bin"));
    }
    for env in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(dir) = std::env::var(env) {
            let root = PathBuf::from(dir);
            roots.push(root.join(r"Windows Kits\10\bin"));
        }
    }
    // 版本号高的优先：同一台机器上往往并存 10.0.19041 / 10.0.22621 / 10.0.26100。
    let mut versions: Vec<(u64, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(v) = version_key(&name) else {
                continue;
            };
            for host in ["x64", "arm64"] {
                let cand = entry.path().join(host).join("rc.exe");
                if cand.is_file() {
                    versions.push((v, cand));
                }
            }
        }
    }
    versions.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    versions.into_iter().next().map(|(_, p)| p)
}

/// `10.0.22621.0` → 可比较的整数键；不是版本号格式的返回 None。
fn version_key(name: &str) -> Option<u64> {
    let mut it = name.split('.');
    let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) else {
        return None;
    };
    let (a, b, c): (u64, u64, u64) = (a.parse().ok()?, b.parse().ok()?, c.parse().ok()?);
    Some(a * 1_000_000_000 + b * 1_000_000 + c)
}

fn main() {
    println!("cargo:rerun-if-changed=linkx.rc");
    println!("cargo:rerun-if-changed=LinkX.ico");

    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    let res = out_dir.join("linkx.res");
    let rc_target = target.ends_with("-msvc");

    let status = if rc_target {
        // rc.exe 吃的是当前目录里的相对路径，`.rc` 里 `1 ICON "LinkX.ico"` 也按当前目录解析，
        // 所以这里不能改 CWD（build.rs 的工作目录就是 crate 根，正合适）。
        let rc = find_rc().unwrap_or_else(|| {
            panic!(
                "msvc 目标 {target} 需要 Windows SDK 的 rc.exe 来编图标资源，没找到。\
                 装 SDK 的 ARM64/x64 组件，或用 LINKX_RC=<rc.exe 全路径> 指定"
            )
        });
        let rc: &Path = &rc;
        let mut cmd = Command::new(rc);
        cmd.arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg("/I")
            .arg(".")
            .arg("linkx.rc");
        cmd.status()
    } else {
        let windres = match target.as_str() {
            "x86_64-pc-windows-gnu" => "x86_64-w64-mingw32-windres",
            "i686-pc-windows-gnu" => "i686-w64-mingw32-windres",
            _ => {
                println!("cargo:warning=未适配的 Windows 目标 {target}，跳过图标资源编译");
                return;
            }
        };
        let mut cmd = Command::new(windres);
        cmd.args(["-O", "coff", "-I", ".", "-i", "linkx.rc", "-o"])
            .arg(&res);
        cmd.status()
    };

    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg={}", res.display()),
        Ok(s) => panic!("编译 linkx.rc 失败（目标 {target}），退出码 {s}"),
        Err(e) => panic!(
            "无法执行资源编译器（目标 {target}）：{e}；\
             gnu 目标需 MinGW 工具链，msvc 目标需 Windows SDK 的 rc.exe"
        ),
    }
}
