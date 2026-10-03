//! 信任库文本格式（`指纹\t名称` 逐行 TSV）——**跨端单一真源**。
//!
//! 设备信任以 **RSA 身份指纹**为锚（不是 MAC，也不是设备名）。两端平台层
//! 只是**存放介质**不同，格式与解析语义必须完全一致——故把解析/序列化/更新规则
//! 收在本模块，平台层与 JNI 桥都调用这里。
//!
//! 规则：指纹是 16 位 ASCII hex，读写统一折叠为小写；名称允许中文与空格，制表符/换行
//! 归一为空格（否则破坏 TSV 结构）；空行 / `#` 注释 / 非法指纹 / 重复指纹一律跳过且绝不
//! panic——信任库损坏最多导致"重配一次"，不能影响主流程。

use crate::engine::TrustedPeer;

/// 指纹长度（SHA-256 前 8 字节 → 16 位小写 hex，与 `linkx_crypto::identity` 同源）
pub const FINGERPRINT_HEX_LEN: usize = 16;

/// 指纹合法性：16 位 ASCII hex（大小写均可，调用方负责折叠）
pub fn is_valid_fingerprint(fp: &str) -> bool {
    fp.len() == FINGERPRINT_HEX_LEN && fp.chars().all(|c| c.is_ascii_hexdigit())
}

/// 解析 TSV → 信任列表（跳过空行 / 注释 / 非法指纹 / 重复项；名称缺省为空串）
pub fn parse_tsv(raw: &str) -> Vec<TrustedPeer> {
    let mut out: Vec<TrustedPeer> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (fp, name) = match line.split_once('\t') {
            Some((f, n)) => (f, n),
            None => (line, ""),
        };
        let fp = fp.trim().to_ascii_lowercase();
        if !is_valid_fingerprint(&fp) {
            continue;
        }
        if out.iter().any(|p| p.fingerprint == fp) {
            continue; // 去重：同指纹保留首条
        }
        out.push(TrustedPeer {
            fingerprint: fp,
            name: name.trim().to_string(),
        });
    }
    out
}

/// 序列化信任列表 → TSV（按指纹升序，便于 diff 与人工核查）
pub fn to_tsv(peers: &[TrustedPeer]) -> String {
    let mut rows: Vec<&TrustedPeer> = peers.iter().collect();
    rows.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
    let mut out = String::new();
    for p in rows {
        let name: String = p
            .name
            .chars()
            .map(|c| {
                if c == '\t' || c == '\n' || c == '\r' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        out.push_str(&p.fingerprint);
        out.push('\t');
        out.push_str(name.trim());
        out.push('\n');
    }
    out
}

/// 新增/更新一条信任（同指纹更新名称；名称为空不覆盖已有名称；非法指纹不入库）
pub fn upsert(peers: &mut Vec<TrustedPeer>, fingerprint: &str, name: &str) {
    let fp = fingerprint.trim().to_ascii_lowercase();
    if !is_valid_fingerprint(&fp) {
        return;
    }
    if let Some(p) = peers.iter_mut().find(|p| p.fingerprint == fp) {
        if !name.trim().is_empty() {
            p.name = name.trim().to_string();
        }
        return;
    }
    peers.push(TrustedPeer {
        fingerprint: fp,
        name: name.trim().to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(fp: &str, name: &str) -> TrustedPeer {
        TrustedPeer {
            fingerprint: fp.to_string(),
            name: name.to_string(),
        }
    }

    /// TSV 往返一致 + 名称中的制表符被归一（不破坏结构）
    #[test]
    fn tsv_roundtrip() {
        let peers = vec![
            peer("3fa76f6244743c27", "Pixel 8 Pro"),
            peer("b091d4e77a2c10ff", "小米\t14"),
        ];
        let back = parse_tsv(&to_tsv(&peers));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].fingerprint, "3fa76f6244743c27");
        assert_eq!(back[1].name, "小米 14");
    }

    /// 非法指纹（长度/字符）必须拒绝；大小写折叠为小写；注释与重复行跳过
    #[test]
    fn tsv_rejects_invalid_and_normalizes_case() {
        let raw = "3FA76F6244743C27\tPixel\n\
                   short\tabc\n\
                   zzzz6f6244743c27\tBad\n\
                   \n\
                   # 注释行\n\
                   3fa76f6244743c27\tDup\n";
        let peers = parse_tsv(raw);
        assert_eq!(peers.len(), 1, "非法/重复行必须被跳过: {peers:?}");
        assert_eq!(peers[0].fingerprint, "3fa76f6244743c27");
        assert_eq!(peers[0].name, "Pixel");
    }

    /// 无名称列的行也能解析（名称 = 空串）
    #[test]
    fn tsv_accepts_fingerprint_only_line() {
        let peers = parse_tsv("3fa76f6244743c27\n");
        assert_eq!(peers.len(), 1);
        assert!(peers[0].name.is_empty());
    }

    /// upsert：同指纹更新名称；空名称不覆盖；非法指纹不入库
    #[test]
    fn upsert_semantics() {
        let mut peers = Vec::new();
        upsert(&mut peers, "3FA76F6244743C27", "Pixel");
        upsert(&mut peers, "3fa76f6244743c27", "Pixel 8 Pro");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "Pixel 8 Pro");
        upsert(&mut peers, "3fa76f6244743c27", "");
        assert_eq!(peers[0].name, "Pixel 8 Pro", "空名称不得覆盖已有名称");
        upsert(&mut peers, "not-a-fp", "X");
        assert_eq!(peers.len(), 1, "非法指纹不得入库");
    }

    /// 指纹校验边界
    #[test]
    fn fingerprint_validation() {
        assert!(is_valid_fingerprint("0123456789abcdef"));
        assert!(is_valid_fingerprint("0123456789ABCDEF"));
        assert!(!is_valid_fingerprint("0123456789abcde")); // 15 位
        assert!(!is_valid_fingerprint("0123456789abcdefg")); // 17 位
        assert!(!is_valid_fingerprint("0123456789abcdeZ")); // 非法字符
        assert!(!is_valid_fingerprint(""));
    }
}
