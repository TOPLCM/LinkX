//! 配对流程：PAIR_CONFIRM(SAS) 互认 + PAIR_DONE 完成。
//!
//! - PAIR_CONFIRM：明文段 = TLV[tag=TAG_SAS, 3B 十进制数字]；除心跳外所有消息都置
//!   ENCRYPTED，故**线上形态为密文 + 16B tag**（SAS 派生自握手哈希，加密后同时得到机密性与完整性）。
//! - PAIR_DONE：Enc(会话密钥, TLV[TAG_FINGERPRINT=对端指纹])。X25519 本身无签名语义，
//!   加密通道即证明双方掌握握手密钥；解密成功 + 指纹与本地握手所得一致 = 完整证据链。

use linkx_crypto::cipher::{decrypt_payload, encrypt_payload, CipherError};
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

    /// 构造 PAIR_CONFIRM 帧体：Enc(session_key, TLV[TAG_SAS])（不含 13B 帧头）。
    /// `dir` = 发送方向域标签；`aad` = 规范帧头（AEAD 的关联数据，改帧头即解密失败）。
    pub fn confirm_payload(
        &self,
        session_key: &[u8; 32],
        dir: u8,
        session_id: &[u8],
        seq: u32,
        aad: &[u8],
    ) -> Result<Vec<u8>, CipherError> {
        encrypt_payload(
            session_key,
            dir,
            session_id,
            seq,
            aad,
            &self.confirm_plaintext(),
        )
    }

    /// 解密并校验对端 PAIR_CONFIRM，是否与我方 SAS 一致
    pub fn verify_confirm(
        &self,
        session_key: &[u8; 32],
        dir: u8,
        session_id: &[u8],
        seq: u32,
        aad: &[u8],
        cipher: &[u8],
    ) -> Result<bool, PairConfirmError> {
        let pt = decrypt_payload(session_key, dir, session_id, seq, aad, cipher)
            .map_err(PairConfirmError::Cipher)?;
        let rc = PairConfirm::parse(&pt).map_err(PairConfirmError::Malformed)?;
        Ok(rc.sas == self.sas)
    }

    /// PAIR_DONE 明文载荷：`TLV[TAG_FINGERPRINT=peer_fp]`
    pub fn pair_done_plaintext(&self, peer_fp: &str) -> Vec<u8> {
        tlv_codec::encode(&[Tlv::buf(TAG_FINGERPRINT, peer_fp.as_bytes())])
            .expect("指纹 TLV 远小于 64B")
    }

    /// 构造 PAIR_DONE：Enc(peer_fingerprint)（基于会话密钥）
    pub fn pair_done_payload(
        &self,
        session_key: &[u8; 32],
        dir: u8,
        session_id: &[u8],
        seq: u32,
        aad: &[u8],
        peer_fp: &str,
    ) -> Result<Vec<u8>, CipherError> {
        encrypt_payload(
            session_key,
            dir,
            session_id,
            seq,
            aad,
            &self.pair_done_plaintext(peer_fp),
        )
    }

    /// 解密并校验 PAIR_DONE；返回对端指纹（与本地 remote_static 指纹比对）
    pub fn verify_pair_done(
        &self,
        session_key: &[u8; 32],
        dir: u8,
        session_id: &[u8],
        seq: u32,
        aad: &[u8],
        cipher: &[u8],
    ) -> Result<String, PairDoneError> {
        let pt = decrypt_payload(session_key, dir, session_id, seq, aad, cipher)
            .map_err(|e| PairDoneError::Cipher(format!("PAIR_DONE 密文校验失败: {e}")))?;
        let fp = tlv_codec::get(&pt, TAG_FINGERPRINT)
            .map_err(|e| PairDoneError::Malformed(e.to_string()))?
            .ok_or(PairDoneError::Malformed("缺少 TAG_FINGERPRINT".into()))?;
        String::from_utf8(fp).map_err(|_| PairDoneError::Malformed("指纹非 UTF-8".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairConfirmError {
    #[error("PAIR_CONFIRM 解密失败: {0}")]
    Cipher(CipherError),
    #[error("PAIR_CONFIRM 载荷畸形: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairDoneError {
    #[error("PAIR_DONE 解密/校验失败: {0}")]
    Cipher(String),
    #[error("PAIR_DONE 载荷畸形: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkx_crypto::fingerprint;
    use linkx_protocol::msg_type;

    // 测试统一用固定方向与 AAD（真实路径由引擎传入帧头）
    const C2S: u8 = linkx_crypto::cipher::DIR_INITIATOR_TO_RESPONDER;
    const S2C: u8 = linkx_crypto::cipher::DIR_RESPONDER_TO_INITIATOR;
    const AAD: &[u8] = b"pair-frame-header";

    #[test]
    fn confirm_encrypt_decrypt_roundtrip() {
        let key = [0x33; 32];
        let sid = b"sid12345";
        let flow_a = PairFlow::new_from_hash(&[0x11; 32]);
        let flow_b = PairFlow::new_from_hash(&[0x11; 32]); // 双端哈希一致 → SAS 一致
        assert_eq!(flow_a.sas, flow_b.sas);

        let ct = flow_a.confirm_payload(&key, C2S, sid, 1, AAD).unwrap();
        assert_eq!(ct.len(), 5 + 16);
        assert!(flow_b.verify_confirm(&key, C2S, sid, 1, AAD, &ct).unwrap());

        // SAS 不一致的对端 → false（仍能成功解密）
        let flow_c = PairFlow::new_from_hash(&[0x22; 32]);
        assert!(!flow_c.verify_confirm(&key, C2S, sid, 1, AAD, &ct).unwrap());

        // 篡改密文 → 完整性校验失败（不 panic）
        let mut bad = ct.clone();
        bad[0] ^= 0x40;
        assert!(flow_c.verify_confirm(&key, C2S, sid, 1, AAD, &bad).is_err());
        // AAD 不一致（帧头被改）→ 完整性校验失败
        assert!(flow_c
            .verify_confirm(&key, C2S, sid, 1, b"other-header", &ct)
            .is_err());
        // 方向不一致 → 失败（域分离）
        assert!(flow_c.verify_confirm(&key, S2C, sid, 1, AAD, &ct).is_err());
        assert!(flow_c
            .verify_confirm(&key, C2S, sid, 1, AAD, &[0x01, 0x02])
            .is_err());
        assert!(flow_c.verify_confirm(&key, C2S, sid, 1, AAD, &[]).is_err());
    }

    #[test]
    fn pair_done_evidence_chain() {
        let sid = b"sid12345";
        let hash = [0x55; 32];
        let sess_key = linkx_crypto::derive_session_key(&hash);
        let fp_r = fingerprint(&[0xBB; 32]); // responder 看到的 initiator 指纹

        let flow_r = PairFlow::new_from_hash(&hash);
        let done = flow_r
            .pair_done_payload(&sess_key, S2C, sid, 10, AAD, &fp_r)
            .unwrap();
        let flow_i = PairFlow::new_from_hash(&hash);
        let got = flow_i
            .verify_pair_done(&sess_key, S2C, sid, 10, AAD, &done)
            .unwrap();
        assert_eq!(got, fp_r);

        let mut bad = done.clone();
        bad[0] ^= 0x40;
        assert!(flow_i
            .verify_pair_done(&sess_key, S2C, sid, 10, AAD, &bad)
            .is_err());
        // 错误 seq 的 nonce → 失败（抗重放/帧复用防护）
        assert!(flow_i
            .verify_pair_done(&sess_key, S2C, sid, 11, AAD, &done)
            .is_err());
    }

    #[test]
    fn first_pair_end_to_end_evidence_chain() {
        use linkx_crypto::noise::run_xx_pair;
        let (h_i, h_r, irs, rrs) = run_xx_pair(&[0xAA; 32], &[0xBB; 32]).unwrap();
        assert_eq!(h_i, h_r, "双方握手哈希必须一致");

        let sess = linkx_crypto::derive_session_key(&h_i);
        assert_eq!(sess, linkx_crypto::derive_session_key(&h_r));
        let fp_responder = fingerprint(&irs); // initiator 视角：对端(responder)长期身份指纹
        let fp_initiator = fingerprint(&rrs); // responder 视角：对端(initiator)长期身份指纹
        let sid = b"sid12345";

        let flow_i = PairFlow::new_from_hash(&h_i);
        let flow_r = PairFlow::new_from_hash(&h_r);
        assert_eq!(flow_i.sas, flow_r.sas, "SAS 一致（人工比对显示同一数字）");

        // PAIR_CONFIRM 双向加密互认：initiator→C2S，responder→S2C
        let c_i = flow_i.confirm_payload(&sess, C2S, sid, 1, AAD).unwrap();
        assert!(flow_r
            .verify_confirm(&sess, C2S, sid, 1, AAD, &c_i)
            .unwrap());
        let c_r = flow_r.confirm_payload(&sess, S2C, sid, 2, AAD).unwrap();
        assert!(flow_i
            .verify_confirm(&sess, S2C, sid, 2, AAD, &c_r)
            .unwrap());

        let d_i = flow_i
            .pair_done_payload(&sess, C2S, sid, 3, AAD, &fp_responder)
            .unwrap();
        assert_eq!(
            flow_r
                .verify_pair_done(&sess, C2S, sid, 3, AAD, &d_i)
                .unwrap(),
            fingerprint(&irs)
        );
        let d_r = flow_r
            .pair_done_payload(&sess, S2C, sid, 4, AAD, &fp_initiator)
            .unwrap();
        assert_eq!(
            flow_i
                .verify_pair_done(&sess, S2C, sid, 4, AAD, &d_r)
                .unwrap(),
            fingerprint(&rrs)
        );

        // 抗重放：PAIR_CONFIRM 的 seq 重复到达 → 滑动窗口拒绝
        let mut w = linkx_crypto::ReplayWindow::default();
        assert!(w.accept(1) && w.accept(2));
        assert!(!w.accept(2), "重复 seq 必须被拒绝");
    }

    #[test]
    fn types_aligned_with_spec() {
        assert_eq!(MSG_PAIR_CONFIRM, msg_type::PAIR_CONFIRM);
        assert_eq!(linkx_protocol::MSG_PAIR_DONE, msg_type::PAIR_DONE);
    }
}
