//! 文件名净化：LocalSend `src/util/filename.rs`（Apache-2.0）的移植版；出处、许可全文与
//! 改动声明见仓库根 `Licenses/localsend/`（Copyright 2022-2026 Tien Do Nam 及贡献者）。
//! vendored 核心与从未被调用的 `ls-interop` 已不在仓库，本文件作为其衍生作品仍在生产路径里。
//! 与上游的改动：去掉 `Options`（替换符/占位符固定为 `_` 与 `untitled`）；只留 `Windows`/`Fat`
//! 两档（落盘端只有 NTFS 与 FAT/exFAT，`Universal` 与 `Windows` 规则完全等价）。
//! 净化**只改写、永不拒绝**：任何对端给的名字都改成一个合法名。拒绝式写法在调用点只推一条 UI
//! 错误就 `return`、不给发送侧回 `FileDone{ok=false}`，于是成为静默丢弃路径——发送侧永远停在
//! "传输中"，用户看到的是"手机发了文件、电脑什么都没有"。

const ILLEGAL_WINDOWS_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 单段文件名的字节上限。**留了 15 字节余量**给收件侧的唯一化后缀（`" (1)"`）：顶格 255 再拼
/// 后缀就越过 NTFS 的 255 字节段长上限，`File::create` 报 os error 123（第二次传同名文件必踩）。
const MAX_LEN: usize = 240;

const REPLACEMENT: &str = "_";
const PLACEHOLDER: &str = "untitled";

/// 按**本机操作系统**选规则（`current()` 走 `cfg!(target_os)`）而非目标卷——收件目录可能在 exFAT U 盘上，但按卷判定得去查文件系统类型；
/// 宁可往别的卷写时多改几个字符，不能落一个写不进去的名字（Windows 这档恰是最严的）。两档非法字符集相同（FAT 沿用 DOS），差别只有保留设备名；尾部 `.`/空格 两档都要压。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rules {
    Windows,
    Fat,
}

impl Rules {
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Fat
        }
    }

    fn is_illegal_char(self, c: char) -> bool {
        // 控制字符（含 NUL、换行）在任何一档都非法；`:` 也必禁——`report:a.txt` 在 NTFS 上不是文件名而是备用数据流（ADS）。
        c.is_control() || ILLEGAL_WINDOWS_CHARS.contains(&c)
    }

    fn checks_reserved(self) -> bool {
        matches!(self, Self::Windows)
    }
}

/// 把对端给的名字改写成 `rules` 下合法的**单个路径段**：分隔符一律换成 `_`，`../../etc/passwd` → `.._.._etc_passwd`，穿越结构在改名时就塌掉了。
pub fn sanitize(name: &str, rules: Rules) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if rules.is_illegal_char(c) {
            result.push_str(REPLACEMENT);
        } else {
            result.push(c);
        }
    }

    collapse_trailing_run(&mut result);
    if rules.checks_reserved() {
        // 改动声明：上游把保留名整个换成 `_`（`con.txt` → `_.txt`），这里改成**加前缀**（`_con.txt`）——名字仍读得出来，不会让人问"我的文件去哪了"。
        if is_reserved_name(&result) {
            result = format!("{REPLACEMENT}{result}");
        }
    }

    truncate_chars(&mut result);

    // 截断可能刚好切在 `.` / 空格之后，留下截断前不存在的尾巴
    collapse_trailing_run(&mut result);
    truncate_chars(&mut result);

    if result.is_empty() || is_relative(&result) {
        result = PLACEHOLDER.to_string();
    }
    result
}

/// 接收侧用这一条：先裁掉首尾空白再按本机落盘规则改名。**永远返回可用名**，因此调用点不存在"拒绝后静默丢弃"这条路径。
pub fn sanitize_file_name(name: &str) -> String {
    sanitize(name.trim(), Rules::current())
}

pub fn is_valid(name: &str, rules: Rules) -> bool {
    if name.is_empty() || name.len() > MAX_LEN || is_relative(name) {
        return false;
    }
    if name.chars().any(|c| rules.is_illegal_char(c)) {
        return false;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    if rules.checks_reserved() && is_reserved_name(name) {
        return false;
    }
    true
}

/// 把 `.` / 空格构成的**尾部连续段**压成一个 `_`：NTFS 会静默丢掉尾部的点和空格，于是 `a.` 与 `a` 撞名，甚至落盘后校验值对不上。
fn collapse_trailing_run(result: &mut String) {
    let trimmed_len = result.trim_end_matches(['.', ' ']).len();
    if trimmed_len != result.len() {
        result.truncate(trimmed_len);
        result.push_str(REPLACEMENT);
    }
}

/// 按字符边界截到 [`MAX_LEN`] 字节（不切进多字节字符，否则 UTF-8 直接坏掉）。
fn truncate_chars(result: &mut String) {
    if result.len() <= MAX_LEN {
        return;
    }
    let mut keep = MAX_LEN;
    while !result.is_char_boundary(keep) {
        keep -= 1;
    }
    result.truncate(keep);
}

fn is_relative(name: &str) -> bool {
    name == "." || name == ".."
}

fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED_WINDOWS_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(name: &str) -> String {
        sanitize(name, Rules::Windows)
    }

    #[test]
    fn ntfs_traps_are_renamed_not_rejected() {
        assert_eq!(win("report:a.txt"), "report_a.txt");
        assert_eq!(win("report."), "report_");
        assert_eq!(win("a  "), "a_");
        assert_eq!(win("con.txt"), "_con.txt");
        assert_eq!(win("LPT9"), "_LPT9");
    }

    #[test]
    fn path_traversal_collapses() {
        assert_eq!(win("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(
            win(r"C:\Windows\system32\cmd.exe"),
            "C__Windows_system32_cmd.exe"
        );
        assert_eq!(win(".."), "_");
        assert_eq!(win("."), "_");
        assert_eq!(win("  "), "_");
    }

    #[test]
    fn sanitize_is_total_and_valid() {
        for name in [
            "",
            " ",
            "...",
            "***",
            "///",
            "\u{1}x",
            "emoji 🎉 文件名",
            "x",
            "con",
        ] {
            let out = win(name);
            assert!(
                is_valid(&out, Rules::Windows),
                "{name:?} → {out:?} 仍不合法（净化函数自身失效）"
            );
        }
    }

    #[test]
    fn long_names_truncate_on_char_boundary() {
        let ascii = "a".repeat(400);
        assert_eq!(win(&ascii).len(), MAX_LEN);
        let cjk = "文".repeat(200); // 每个 3 字节
        let out = win(&cjk);
        assert!(out.len() <= MAX_LEN, "超出字节上限: {}", out.len());
        assert!(out.ends_with('文'), "截断切进了多字节字符: {out:?}");
    }

    #[test]
    fn fat_rules_differ_only_in_reserved_names() {
        assert_eq!(sanitize("con.txt", Rules::Fat), "con.txt");
        assert_eq!(sanitize("a:b", Rules::Fat), "a_b");
        assert_eq!(sanitize("report.", Rules::Fat), "report_");
        assert_eq!(sanitize("...", Rules::Fat), "_");
    }

    #[test]
    fn leaves_room_for_uniqueness_suffix() {
        let out = win(&"a".repeat(500));
        assert!(
            out.len() + " (99)".len() <= 255,
            "拼后缀后越过 NTFS 段长: {}",
            out.len()
        );
    }

    #[test]
    fn ordinary_names_pass_through() {
        for name in [
            "季度汇报.pptx",
            "IMG_20260924_193012.jpg",
            "report-final_v2.pdf",
            "会议 纪要（终版）.docx",
        ] {
            assert_eq!(win(name), name, "正常名字被改坏了");
        }
    }
}
