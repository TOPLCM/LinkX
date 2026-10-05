//! 配对流程：PAIR_CONFIRM(SAS) 互认 + PAIR_DONE 完成。
//!
//! - PAIR_CONFIRM：明文段 = TLV[tag=TAG_SAS, 3B 十进制数字]；除心跳外所有消息都置
//!   ENCRYPTED，故**线上形态为密文 + 16B tag**（SAS 派生自握手哈希，加密后同时得到机密性与完整性）。
//! - PAIR_DONE：Enc(会话密钥, TLV[TAG_FINGERPRINT=对端指纹])。X25519 本身无签名语义，
//!   加密通道即证明双方掌握握手密钥；解密成功 + 指纹与本地握手所得一致 = 完整证据链。

use linkx_protocol::tlv_codec::{self, Tlv};
use linkx_protocol::{MSG_PAIR_CONFIRM, TAG_FINGERPRINT, TAG_SAS};

/// PAIR_CONFIRM 消息的 SAS 载荷
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairConfirm {
    pub sas: u32,
}

impl PairConfirm {
    pub fn encode(&self) -> Vec<u8> {
        let digits = [
            ((self.sas >> 16) & 0xFF) as u8,
            ((self.sas >> 8) & 0xFF) as u8,
            (self.sas & 0xFF) as u8,
        ];
        tlv_codec::simple(TAG_SAS, digits)
    }

    pub fn parse(buf: &[u8]) -> Result<Self, String> {
        let v = tlv_codec::get(buf, TAG_SAS)
            .map_err(|e| format!("TLV 解析失败: {e}"))?
            .ok_or("缺少 TAG_SAS")?;
        if v.len() != 3 {
            return Err(format!("SAS 应 3B，实际 {}", v.len()));
        }
        let sas = ((v[0] as u32) << 16) | ((v[1] as u32) << 8) | v[2] as u32;
        Ok(Self { sas })
    }
}

#[derive(Debug, Clone)]
pub struct PairFlow {
    pub msg_type: u8,
    pub sas: u32,
}

impl PairFlow {
    /// 由本地握手哈希派生 SAS
    pub fn new_from_hash(hash: &[u8; 32]) -> Self {
        Self {
            msg_type: MSG_PAIR_CONFIRM,
            sas: linkx_crypto::sas_digits(hash),
        }
    }

    /// PAIR_CONFIRM 明文载荷：`TLV[TAG_SAS, 3B]`（帧头/加解密由调用方统一处理）
    pub fn confirm_plaintext(&self) -> Vec<u8> {
        PairConfirm { sas: self.sas }.encode()
    }

    /// PAIR_DONE 明文载荷：`TLV[TAG_FINGERPRINT=peer_fp]`
    pub fn pair_done_plaintext(&self, peer_fp: &str) -> Vec<u8> {
        tlv_codec::encode(&[Tlv::buf(TAG_FINGERPRINT, peer_fp.as_bytes())])
            .expect("指纹 TLV 远小于 64B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkx_protocol::msg_type;

    /// 引擎真正用到的就这两条编解码（加解密与重放窗口在帧层，见 engine 的用例）
    #[test]
    fn confirm_and_done_plaintext_shapes() {
        let flow = PairFlow::new_from_hash(&[0x11; 32]);
        assert_eq!(flow.sas, linkx_crypto::sas_digits(&[0x11; 32]));
        assert!(flow.sas < 1_000_000, "SAS 是 6 位十进制展示码");
        assert_eq!(
            PairConfirm::parse(&flow.confirm_plaintext()).unwrap(),
            PairConfirm { sas: flow.sas }
        );
        assert_eq!(
            tlv_codec::get(
                &flow.pair_done_plaintext("aabbccddeeff0011"),
                TAG_FINGERPRINT
            )
            .unwrap(),
            Some("aabbccddeeff0011".as_bytes().to_vec())
        );
    }

    /// SAS 载荷来自对端写的那几个字节：畸形时必须报错而不是 panic
    #[test]
    fn confirm_parse_rejects_malformed() {
        assert!(PairConfirm::parse(&tlv_codec::simple(TAG_SAS, [1u8, 2u8])).is_err());
        assert!(PairConfirm::parse(&tlv_codec::simple(TAG_SAS, [1u8, 2u8, 3u8, 4u8])).is_err());
        assert!(PairConfirm::parse(&[]).is_err());
    }

    #[test]
    fn types_aligned_with_spec() {
        assert_eq!(MSG_PAIR_CONFIRM, msg_type::PAIR_CONFIRM);
        assert_eq!(linkx_protocol::MSG_PAIR_DONE, msg_type::PAIR_DONE);
    }
}
