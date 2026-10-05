//! fuzz 目标 4：接收侧文件名净化对任意输入都不越界、不留下穿越结构。
//! 这条路径吃的是**对端设备写来的字符串**，改名式（永不拒绝）意味着它是收件目录唯一的护栏。
#![no_main]
use libfuzzer_sys::fuzz_target;
use linkx_transfer::filename::{sanitize, Rules};

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    for rules in [Rules::Windows, Rules::Fat] {
        let out = sanitize(&name, rules);
        // 分隔符与纯相对路径段必须在改名时塌掉，落盘端拼出来的仍是收件目录里的一个文件
        assert!(!out.contains('/'), "含正斜杠: {out:?}");
        assert!(!out.contains('\\'), "含反斜杠: {out:?}");
        assert!(out != "." && out != "..", "留下了相对路径段");
        // 顶格的名字会撞 NTFS 的 255 字节段长上限，唯一化后缀就写不进去
        assert!(out.len() <= 240, "超出单段上限: {} 字节", out.len());
        assert!(!out.is_empty(), "净化不许交出空名字");
        // 再净一次必须一模一样：否则会"A 发给 B 改了名、B 再转发又变一次名"
        let again = sanitize(&out, rules);
        assert_eq!(again, out, "净化不自幂（{name:?} → {out:?} → {again:?}）");
    }
});
