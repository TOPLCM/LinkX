//! 开机自启动：`HKCU\...\CurrentVersion\Run` 下那一条 `LinkX` 值的构造、识别与读写。
//!
//! 界面上那个开关显示的必须是**注册表里的真值**：用户可能直接在注册表里改，安全软件也会清掉或
//! 改写启动项，`settings.ini` 里那份只是上一次的记忆。所以字符串的构造/解析留在纯函数里（可单测），
//! 注册表读写单独成一层（只有真机副作用，不进单测）。

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Run 键下本壳占用的值名
const RUN_VALUE: &str = "LinkX";
/// 开机拉起来时带的参数：那一个实例不弹主窗口（要的是"后台待着"，不是"再开一个界面"）
pub(crate) const MINIMIZED_ARG: &str = "--minimized";

/// 注册表里那一条启动项现在的样子
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEntry {
    /// 没有这一条（或值是空的）
    Missing,
    /// 就是本机这个 exe，且带 `--minimized`
    Ours,
    /// 指向本机 exe，但参数里认不出 `--minimized`
    NotMinimized,
    /// 指向别的程序：启动项被外部改写过
    Foreign,
}

impl RunEntry {
    /// 开机后到底会不会拉起 LinkX —— 界面开关按这个显示，不按 `settings.ini`
    pub(crate) fn enabled(self) -> bool {
        self != RunEntry::Missing
    }

    /// 内容是否与本机现在该写的一致；不一致时开关旁要说实话，不能声称"已按本机设置开启"
    pub(crate) fn is_ours(self) -> bool {
        self == RunEntry::Ours
    }
}

/// 写进 Run 键的值：exe 路径**必须加引号**——安装目录常常带空格，不加引号会被拆成两段
pub(crate) fn run_value_data(exe: &str) -> String {
    format!("\"{exe}\" {MINIMIZED_ARG}")
}

/// 命令行拆成 (程序路径, 参数串)：引号包裹的路径可以带空格，裸路径按第一个空白切
fn split_command(raw: &str) -> Option<(&str, &str)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => Some((rest[..end].trim(), rest[end + 1..].trim())),
            // 引号没闭合就看不出要跑什么：整串当路径，判成"不是本机写的"。有一条开不起来
            // 的启动项也不能谎报成"没开"
            None => Some((rest.trim(), "")),
        };
    }
    let cut = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..cut], s[cut..].trim()))
}

/// Windows 路径不区分大小写，末尾的分隔符也不算另一个路径
fn same_path(a: &str, b: &str) -> bool {
    norm_path(a).eq_ignore_ascii_case(&norm_path(b))
}

fn norm_path(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .to_string()
}

/// 把注册表里现存的值（`None` = 读不到这一条）判成 [`RunEntry`]
pub(crate) fn classify_run_value(raw: Option<&str>, exe: &str) -> RunEntry {
    let Some((path, args)) = raw.and_then(split_command) else {
        return RunEntry::Missing;
    };
    if !same_path(path, exe) {
        return RunEntry::Foreign;
    }
    if args
        .split_whitespace()
        .any(|a| a.eq_ignore_ascii_case(MINIMIZED_ARG))
    {
        RunEntry::Ours
    } else {
        RunEntry::NotMinimized
    }
}

/// 参数里有没有 `--minimized`（纯函数，`main` 用它决定要不要显示主窗口）
pub(crate) fn has_minimized_arg(args: &[String]) -> bool {
    args.iter().any(|a| a == MINIMIZED_ARG)
}

/// 本壳 exe 的绝对路径：运行时查，不写死安装路径
fn exe_path() -> Result<String, String> {
    let p = std::env::current_exe().map_err(|e| format!("取当前程序路径失败: {e}"))?;
    // 这一层按 UTF-16 写注册表；路径含无法成串的码点时宁可报错，也不写半截路径出去
    p.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "当前程序路径含无法写入注册表的字符".to_string())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win_err(what: &str, rc: WIN32_ERROR) -> String {
    format!("{what}失败（系统错误 {}）", rc.0)
}

/// 注册表里那条启动项的实际状态。`Err` = 读不了 —— 读不了不等于"没开"，界面不许按"没开"显示
pub(crate) fn observe() -> Result<RunEntry, String> {
    let exe = exe_path()?;
    Ok(classify_run_value(read_value()?.as_deref(), &exe))
}

fn read_value() -> Result<Option<String>, String> {
    let subkey = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);
    let (sub, val) = (PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr()));
    let mut cch = 0u32;
    let probe = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub,
            val,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut cch),
        )
    };
    if probe == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if probe.is_err() {
        return Err(win_err("读取开机启动项", probe));
    }
    // 第二次调用才真取数据：pcbData 给的是**字节数**（含结尾 NUL），按它分配 u16 缓冲，
    // 容量天然是一倍余量；再把这块缓冲的真实字节数告诉 API。
    let mut buf = vec![0u16; (cch as usize).max(1)];
    let mut bytes = cch * 2;
    let got = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub,
            val,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut bytes),
        )
    };
    if got.is_err() {
        return Err(win_err("读取开机启动项", got));
    }
    let len = ((bytes / 2) as usize).min(buf.len());
    Ok(Some(
        String::from_utf16_lossy(&buf[..len])
            .trim_matches('\0')
            .to_string(),
    ))
}

/// 按界面上的开关落子：开 = 写入本机命令，关 = 删除这一条
pub(crate) fn apply(on: bool) -> Result<(), String> {
    let subkey = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);
    let (sub, val) = (PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr()));
    let rc = if on {
        let data = wide(&run_value_data(&exe_path()?));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                sub,
                val,
                REG_SZ.0,
                Some(data.as_ptr() as *const c_void),
                (data.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, sub, val) }
    };
    // 关的时候"本来就没有"就是成功：注册表已经是用户要的那个状态
    if rc == ERROR_FILE_NOT_FOUND && !on {
        return Ok(());
    }
    if rc.is_err() {
        return Err(win_err(
            if on {
                "写入开机启动项"
            } else {
                "清除开机启动项"
            },
            rc,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\LinkX\linkx.exe";

    #[test]
    fn run_value_quotes_a_path_with_spaces_and_adds_the_flag() {
        assert_eq!(
            run_value_data(EXE),
            r#""C:\Program Files\LinkX\linkx.exe" --minimized"#
        );
        assert_eq!(
            run_value_data(r"C:\LinkX\linkx.exe"),
            r#""C:\LinkX\linkx.exe" --minimized"#
        );
    }

    /// 自己写的值必须认得回来：认不回来就会把"已开启"显示成"未开启"
    #[test]
    fn run_value_written_by_us_classifies_as_ours() {
        let data = run_value_data(EXE);
        assert_eq!(classify_run_value(Some(&data), EXE), RunEntry::Ours);
        assert!(RunEntry::Ours.enabled() && RunEntry::Ours.is_ours());
        // 参数顺序与多余空格都不该改变判定
        assert_eq!(
            classify_run_value(Some(&format!("\"{EXE}\"   --minimized ")), EXE),
            RunEntry::Ours
        );
    }

    #[test]
    fn missing_or_blank_value_is_off() {
        for raw in [None, Some(""), Some("   ")] {
            assert_eq!(classify_run_value(raw, EXE), RunEntry::Missing);
            assert!(!RunEntry::Missing.enabled());
        }
    }

    /// 值被外部改掉的两条路：丢了参数（开机仍然弹窗）、换了程序（开的不是 LinkX）
    #[test]
    fn externally_edited_value_is_detected() {
        assert_eq!(
            classify_run_value(Some(&format!("\"{EXE}\"")), EXE),
            RunEntry::NotMinimized
        );
        assert_eq!(
            classify_run_value(Some(r#""C:\Other\tool.exe" --minimized"#), EXE),
            RunEntry::Foreign
        );
        // 指向别处也算"有启动项"，但不能说成"本机这套"
        assert!(RunEntry::Foreign.enabled() && !RunEntry::Foreign.is_ours());
        assert!(RunEntry::NotMinimized.enabled() && !RunEntry::NotMinimized.is_ours());
    }

    /// 没加引号的可执行路径带空格 = 系统只会跑到 `C:\Program`，这一条根本起不来：
    /// 判成"不是本机写的"比判成"已按本机设置开启"诚实
    #[test]
    fn unquoted_path_is_only_readable_when_it_has_no_spaces() {
        assert_eq!(classify_run_value(Some(EXE), EXE), RunEntry::Foreign);
        let plain = r"C:\LinkX\linkx.exe";
        assert_eq!(
            classify_run_value(Some(plain), plain),
            RunEntry::NotMinimized
        );
        assert_eq!(
            classify_run_value(Some(&format!("{plain} {MINIMIZED_ARG}")), plain),
            RunEntry::Ours
        );
    }

    /// 路径判定按 Windows 口径：大小写与末尾分隔符都不算两条
    #[test]
    fn path_compare_follows_windows_rules() {
        assert_eq!(
            classify_run_value(
                Some(r#""C:\PROGRAM FILES\linkx\LINKX.EXE" --minimized"#),
                EXE
            ),
            RunEntry::Ours
        );
        assert_eq!(
            classify_run_value(
                Some(r#""C:\Program Files\LinkX\linkx.exe\\" --minimized"#),
                EXE
            ),
            RunEntry::Ours
        );
        // 引号没闭合 = 看不出要跑什么，只能说"这条不是本机写的"
        assert_eq!(
            classify_run_value(
                Some(r#""C:\Program Files\LinkX\linkx.exe --minimized"#),
                EXE
            ),
            RunEntry::Foreign
        );
    }

    #[test]
    fn minimized_flag_recognized_only_as_a_whole_argument() {
        assert!(has_minimized_arg(&["--minimized".to_string()]));
        assert!(has_minimized_arg(&[
            r"C:\Users\a\Desktop\x.pdf".to_string(),
            "--minimized".to_string()
        ]));
        assert!(!has_minimized_arg(&[]));
        assert!(!has_minimized_arg(&["--minimize".to_string()]));
        assert!(
            !has_minimized_arg(&["C:\\linkx.exe --minimized".to_string()]),
            "整串命令行不是单个参数，不该被当成带了这个开关"
        );
    }
}
