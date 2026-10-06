//! fuzz 目标 4：接收侧文件名净化对任意输入都不越界、不留下穿越结构。
//! 这条路径吃的是**对端设备写来的字符串**，改名式（永不拒绝）意味着它是收件目录唯一的护栏。
//!
//! 覆盖到的判据都同时写在 `filename.rs` 的单元测试里（CI 跑那份，fuzz 跑同一批断言的随机版）：
//! `corpus/sanitize_filename/` 里放了刻意的种子（盘符、ADS 冒号、保留设备名、URL 编码的 `..`、
//! 尾部点串），因为这个函数几乎不分支，覆盖率引导自己长不出这些输入。
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_transfer::filename::{is_valid, sanitize, sanitize_file_name, Rules, MAX_LEN};

/// 落盘端拼路径时用的分隔符与 NTFS 的流分隔符：留下任何一个就不再是"一个文件名"
const FORBIDDEN: &[char] = &['/', '\\', ':'];

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    for rules in [Rules::Windows, Rules::Fat] {
        let out = sanitize(&name, rules);
        for c in FORBIDDEN {
            assert!(
                !out.contains(c),
                "{name:?} → {out:?} 里留了 {c:?}"
            );
        }
        assert!(out != "." && out != "..", "留下了相对路径段");
        assert!(out.len() <= MAX_LEN, "超出单段上限: {} 字节", out.len());
        assert!(!out.is_empty(), "净化不许交出空名字");
        assert!(
            !out.chars().any(char::is_control),
            "控制字符没清干净: {out:?}"
        );
        // 尾部不能是点或空格：NTFS 会静默吃掉它们，于是 `a.` 与 `a` 撞名、校验值对不上
        assert!(
            !out.ends_with('.') && !out.ends_with(' '),
            "留了会被文件系统静默丢弃的尾巴: {out:?}"
        );
        // 保留设备名只在 Windows 这档管：`con.txt` 在 NTFS 上根本创建不出来
        if rules == Rules::Windows {
            let stem = out.split('.').next().unwrap_or(&out);
            assert!(
                !matches!(
                    stem.to_ascii_lowercase().as_str(),
                    "con"
                        | "prn"
                        | "aux"
                        | "nul"
                        | "com1"
                        | "com2"
                        | "com3"
                        | "com4"
                        | "com5"
                        | "com6"
                        | "com7"
                        | "com8"
                        | "com9"
                        | "lpt1"
                        | "lpt2"
                        | "lpt3"
                        | "lpt4"
                        | "lpt5"
                        | "lpt6"
                        | "lpt7"
                        | "lpt8"
                        | "lpt9"
                ),
                "保留了 NTFS 设备名: {out:?}"
            );
        }
        // 再净一次必须一模一样：否则会"A 发给 B 改了名、B 再转发又变一次名"
        let again = sanitize(&out, rules);
        assert_eq!(again, out, "净化不自幂（{name:?} → {out:?} → {again:?}）");
        // 产物必须直接通过合法性判定：否则"净化"与"校验"两套口径互相不认
        assert!(is_valid(&out, rules), "净化产物过不了自己的校验: {out:?}");
    }
    // 生产入口（按本机规则 + 先裁首尾空白）同判据：调用点用的就是它，不是上面那两个参数化函数
    let live = sanitize_file_name(&name);
    assert!(!live.is_empty() && live.len() <= MAX_LEN, "生产入口交出怪名字");
    assert_eq!(sanitize(live.trim(), Rules::current()), live, "生产入口不自幂");
});
