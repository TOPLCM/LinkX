//! 通知正文里的验证码提取：这是一条**规则**而不是界面细节，两端各写一份迟早出现
//! "电脑上认得出、手机上认不出"。规则刻意保守：
//! 1. **必须有语境关键词**（验证码 / code / OTP…），否则只当普通数字，绝不返回结果——
//!    通知里 4~8 位数字太多（订单号、金额、楼层、日期），宁可少给一个按钮。
//! 2. 数字串 **4..=8** 位且**前后不得再贴着数字**：11 位手机号不会被误切成"一段 4 位码"。
//! 3. 串内允许单个空格或连字符分隔（`12 34 56`、`123-456`），返回时去掉分隔符。
//! 4. 多个候选时取**离关键词最近**的那个。

/// 认得的关键词（小写匹配；中英混排都覆盖到）
const KEYWORDS: &[&str] = &[
    "验证码",
    "校验码",
    "检查码",
    "动态码",
    "动态密码",
    "一次性密码",
    "一次性验证码",
    "提取码",
    "取件码",
    "激活码",
    "授权码",
    "安全码",
    "短信码",
    "登录码",
    "口令",
    "code",
    "otp",
    "passcode",
    "one-time",
    "verification",
    "security code",
];

/// 数字与关键词之间允许的最大字符距离（两侧同宽）。
const CODE_WINDOW: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtpCode {
    /// 去掉分隔符后的纯数字
    pub digits: String,
    /// 命中的关键词（便于两端展示同一句解释文案，也让调试能回答"凭什么认为这是码"）
    pub keyword: &'static str,
}

/// 从 `title + text` 里提取验证码；没有可信结果返回 `None`。
/// 位置一律按 **char** 计（不按字节）：中文按 UTF-8 存，字节偏移会把距离算成 3 倍。
pub fn extract_code(title: &str, text: &str) -> Option<OtpCode> {
    // 关键词下标和数字下标必须出自同一份字符串：`to_lowercase` 对个别字符会改变 char 个数
    // （土耳其语 İ 折叠成 i + 组合点 = 2 个 char），两份字符串错位后"离关键词多远"就算错了
    let hay = format!("{title} {text}").to_lowercase();
    let chars: Vec<char> = hay.chars().collect();
    let mut hits: Vec<(usize, &'static str)> = Vec::new();
    for kw in KEYWORDS {
        let pat: Vec<char> = kw.chars().collect();
        if pat.is_empty() {
            continue;
        }
        let mut i = 0usize;
        while i + pat.len() <= chars.len() {
            if chars[i..i + pat.len()] == pat[..] {
                hits.push((i, kw));
                i += pat.len();
            } else {
                i += 1;
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    let runs = digit_runs(&hay);
    let mut best: Option<(usize, OtpCode)> = None; // (与关键词的距离, 结果)
    for (kw_at, kw) in hits {
        for (run_at, digits) in &runs {
            let len = digits.len();
            if !(4..=8).contains(&len) {
                continue;
            }
            // 两种语序都要能中："验证码 123456" 与 "123456 是您的验证码"。两侧同宽取 40：
            // 收紧到 16 会把 "182734 is your Google verification code" 判没。
            let behind = kw_at.saturating_sub(*run_at); // 数字在关键词之前多远
            let ahead = run_at.saturating_sub(kw_at); // 数字在关键词之后多远
                                                      // `behind`/`ahead` 是 saturating 的，两者必有一个是 0：写成
                                                      // `!(behind <= N || ahead <= N)` 会恒为 false，等于根本没有窗口。
            let outside = if *run_at >= kw_at {
                ahead > CODE_WINDOW
            } else {
                behind > CODE_WINDOW
            };
            if outside {
                continue;
            }
            let dist = if *run_at >= kw_at {
                *run_at - kw_at
            } else {
                (kw_at - *run_at) + len // 同距离时优先"关键词在后"的读法（更符合中文习惯）
            };
            let cand = OtpCode {
                digits: digits.clone(),
                keyword: kw,
            };
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, cand));
            }
        }
    }
    best.map(|(_, c)| c)
}

/// 扫出所有"数字串"，允许串内单个空格/连字符分隔；返回 (起始下标, 去掉分隔符后的数字)。
fn digit_runs(s: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut digits = String::new();
        let mut end = i;
        while end < chars.len() {
            let c = chars[end];
            if c.is_ascii_digit() {
                digits.push(c);
                end += 1;
            } else if (c == ' ' || c == '-' || c == '\u{a0}')
                && end + 1 < chars.len()
                && chars[end + 1].is_ascii_digit()
            {
                // 分隔符只在"两边都是数字"时才算串内分隔
                let mut jump = end + 1;
                while jump < chars.len() && (chars[jump] == ' ' || chars[jump] == '\u{a0}') {
                    jump += 1;
                }
                if jump < chars.len() && chars[jump].is_ascii_digit() && !digits.is_empty() {
                    end = jump;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        // 要挡的是"串尾贴着另一个数字"（分隔符吃进下一段）；更长的串整体按长度过滤
        let tail_glued = end < chars.len() && chars[end].is_ascii_digit();
        if !tail_glued && digits.len() >= 3 {
            out.push((start, digits));
        }
        i = end.max(start + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(t: &str) -> Option<String> {
        extract_code("", t).map(|c| c.digits)
    }

    #[test]
    fn chinese_common_shapes() {
        assert_eq!(code("验证码：123456").as_deref(), Some("123456"));
        assert_eq!(
            code("您的验证码是 8890，5 分钟内有效").as_deref(),
            Some("8890")
        );
        assert_eq!(code("1234 是您的动态密码").as_deref(), Some("1234"));
        assert_eq!(
            code("【某某银行】校验码 6 7 8 9 0，请勿泄露").as_deref(),
            Some("67890")
        );
        assert_eq!(code("快递取件码：4-3-2-1").as_deref(), Some("4321"));
    }

    #[test]
    fn english_shapes() {
        assert_eq!(code("Your code is 4321").as_deref(), Some("4321"));
        assert_eq!(
            code("182734 is your Google verification code").as_deref(),
            Some("182734")
        );
        assert_eq!(code("OTP: 90 210").as_deref(), Some("90210"));
    }

    #[test]
    fn refuses_without_keyword() {
        assert_eq!(code("订单 123456 已发货"), None);
        assert_eq!(code("您于 1430 楼层签到"), None);
        assert_eq!(code("13800001111"), None);
    }

    #[test]
    fn rejects_runs_inside_longer_numbers() {
        assert_eq!(code("验证码 13800001111"), None); // 11 位手机号冒充不了 4~8 位码
        assert_eq!(code("验证码 123456789"), None); // 9 位，超出可信长度
    }

    #[test]
    fn picks_run_nearest_keyword() {
        assert_eq!(
            code("2026 年 9 月 28 日 10:30，您的验证码是 668899").as_deref(),
            Some("668899")
        );
    }

    #[test]
    fn title_counts_as_context() {
        let c =
            extract_code("短信验证码", "6 6 8 8 9 9 请在 5 分钟内使用").expect("标题里就该给语境");
        assert_eq!(c.digits, "668899");
        assert_eq!(c.keyword, "验证码");
    }

    /// İ 折叠成 2 个 char：关键词下标按折叠后的串量、数字下标按折叠前的串量，两者会错开一个
    /// 偏移，40 字符的距离窗口被莫名放宽。本例正好卡在界外一位。
    #[test]
    fn case_folding_does_not_shift_the_distance_window() {
        let text = format!("code{}123456", " ".repeat(37));
        assert_eq!(
            extract_code("XXXXXXXX", &text).map(|c| c.digits),
            None,
            "前提：等长 ASCII 标题下这个距离在窗口外"
        );
        assert_eq!(
            extract_code("İİİİ", &text).map(|c| c.digits),
            None,
            "4 个 İ 不许把界外的码挪进窗"
        );
        // 界内一位的对照：不是"这种写法一律不认"
        let inside = format!("code{}123456", " ".repeat(36));
        assert_eq!(
            extract_code("İİİİ", &inside).map(|c| c.digits),
            Some("123456".to_string())
        );
    }
}
