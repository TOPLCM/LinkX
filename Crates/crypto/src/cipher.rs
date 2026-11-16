//! 消息加密：ChaCha20-Poly1305，帧布局 = 帧头 + 密文 + 16B tag。
//! nonce = HMAC-SHA256(session_key, direction || seq_be || session_id)[0..12]（96-bit，RFC 8439）。
//!
//! **方向域分离**：双方共用同一 `session_key`/`session_id` 且 `seq` 同起点，派生不含方向时
//! 两个方向会撞 `(key, nonce)` → ChaCha20 密钥流复用 → 可异或恢复明文；故加入 1B 方向标签。
//!
//! **帧头认证**：13B 帧头作为 AEAD 的 AAD，`msg_type`/`flags` 因此受完整性保护
//! （仅靠 nonce 绑定 `seq` 拦不住 `flags` 篡改）。

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

use crate::noise::NOISE_KEY_LEN;

type HmacSha256 = Hmac<Sha256>;

/// 方向域标签：参与 nonce 派生的 1B 输入（initiator→responder）
pub const DIR_INITIATOR_TO_RESPONDER: u8 = 0x01;
/// 方向域标签（responder→initiator）
pub const DIR_RESPONDER_TO_INITIATOR: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CipherError {
    #[error("解密失败：完整性校验未通过（tag/密文被篡改）")]
    Integrity,
    #[error("密钥长度非法")]
    BadKeyLen,
}

/// 派生每帧独立 nonce（方向参与派生，保证双向不撞车）：
/// `HMAC(session_key, direction || seq_be || session_id)[0..12]`
pub fn derive_nonce(
    session_key: &[u8; NOISE_KEY_LEN],
    direction: u8,
    seq: u32,
    session_id: &[u8],
) -> [u8; 12] {
    let mut mac = match <HmacSha256 as Mac>::new_from_slice(session_key) {
        Ok(m) => m,
        Err(_) => unreachable!("HMAC-SHA256 接受任意长度 key，32B 输入必然成功"),
    };
    mac.update(&[direction]);
    mac.update(&seq.to_be_bytes());
    mac.update(session_id);
    let out = mac.finalize().into_bytes();
    let mut n = [0u8; 12];
    n.copy_from_slice(&out[..12]);
    n
}

/// 加密 payload → ciphertext || tag(16B)；帧头作为 aad 传入
pub fn encrypt_payload(
    session_key: &[u8; NOISE_KEY_LEN],
    direction: u8,
    session_id: &[u8],
    seq: u32,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CipherError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(session_key));
    let nonce_bytes = derive_nonce(session_key, direction, seq, session_id);
    let nonce = Nonce::from_slice(&nonce_bytes);
    cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CipherError::Integrity)
}

/// 解密 ciphertext_with_tag（含尾部 16B tag）；aad 须与加密时逐字节一致
pub fn decrypt_payload(
    session_key: &[u8; NOISE_KEY_LEN],
    direction: u8,
    session_id: &[u8],
    seq: u32,
    aad: &[u8],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, CipherError> {
    if ciphertext_with_tag.len() < 16 {
        return Err(CipherError::Integrity);
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(session_key));
    let nonce_bytes = derive_nonce(session_key, direction, seq, session_id);
    let nonce = Nonce::from_slice(&nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext_with_tag,
                aad,
            },
        )
        .map_err(|_| CipherError::Integrity)
}

#[cfg(test)]
mod tests {
    use super::*;

    const C2S: u8 = DIR_INITIATOR_TO_RESPONDER;
    const S2C: u8 = DIR_RESPONDER_TO_INITIATOR;

    #[test]
    fn roundtrip_and_tamper_detect() {
        let key = [0x42; 32];
        let sid = b"01234567"; // 8B session_id
        let pt = b"hello linkx";
        let aad = b"frame-header-13";
        let ct = encrypt_payload(&key, C2S, sid, 1, aad, pt).unwrap();
        assert_eq!(ct.len(), pt.len() + 16);

        let back = decrypt_payload(&key, C2S, sid, 1, aad, &ct).unwrap();
        assert_eq!(back, pt);

        let mut tampered = ct.clone();
        tampered[0] ^= 0x01;
        assert!(matches!(
            decrypt_payload(&key, C2S, sid, 1, aad, &tampered),
            Err(CipherError::Integrity)
        ));
        let mut tag_tampered = ct.clone();
        let n = tag_tampered.len() - 1;
        tag_tampered[n] ^= 0x80;
        assert!(matches!(
            decrypt_payload(&key, C2S, sid, 1, aad, &tag_tampered),
            Err(CipherError::Integrity)
        ));
    }

    #[test]
    fn nonce_varies_with_seq_and_session() {
        let key = [0x07; 32];
        let n1 = derive_nonce(&key, C2S, 1, b"aaaa1111");
        let n2 = derive_nonce(&key, C2S, 2, b"aaaa1111");
        let n3 = derive_nonce(&key, C2S, 1, b"bbbb2222");
        assert_ne!(n1, n2);
        assert_ne!(n1, n3);
        // 同 seq 同 session 同 key → 相同 nonce（确定性，可用于重放窗口校验）
        assert_eq!(
            derive_nonce(&key, C2S, 5, b"aaaa1111"),
            derive_nonce(&key, C2S, 5, b"aaaa1111")
        );
    }

    /// 双向 nonce 必须不同（方向域分离的核心断言）
    #[test]
    fn nonce_differs_by_direction() {
        let key = [0x99; 32];
        let sid = b"same-sid";
        let c2s = derive_nonce(&key, C2S, 1, sid);
        let s2c = derive_nonce(&key, S2C, 1, sid);
        assert_ne!(c2s, s2c, "双向同 (key,seq,session_id) 不得派生出相同 nonce");

        let ct_c2s = encrypt_payload(&key, C2S, sid, 1, b"hdr", b"same-plaintext").unwrap();
        let ct_s2c = encrypt_payload(&key, S2C, sid, 1, b"hdr", b"same-plaintext").unwrap();
        assert_ne!(ct_c2s, ct_s2c, "方向不同则密文必须不同（否则密钥流复用）");

        // 跨方向解密必须失败（域分离生效）
        assert!(decrypt_payload(&key, S2C, sid, 1, b"hdr", &ct_c2s).is_err());
        assert!(decrypt_payload(&key, C2S, sid, 1, b"hdr", &ct_s2c).is_err());
    }

    #[test]
    fn aad_tampering_detected() {
        let key = [0x11; 32];
        let sid = b"sid-8byt";
        let aad = b"hdr:type=10,flags=10";
        let ct = encrypt_payload(&key, C2S, sid, 3, aad, b"payload").unwrap();
        assert!(decrypt_payload(&key, C2S, sid, 3, aad, &ct).is_ok());
        // 篡改 AAD 或缺失 → 完整性校验失败（模拟 msg_type/flags 被改）
        assert!(matches!(
            decrypt_payload(&key, C2S, sid, 3, b"hdr:type=11,flags=10", &ct),
            Err(CipherError::Integrity)
        ));
        assert!(matches!(
            decrypt_payload(&key, C2S, sid, 3, &[], &ct),
            Err(CipherError::Integrity)
        ));
    }

    #[test]
    fn replay_bytes_unchanged() {
        let key = [0x01; 32];
        let pt = vec![0xAA; 100];
        let ct1 = encrypt_payload(&key, C2S, b"sid8bytes", 7, b"h", &pt).unwrap();
        let ct2 = encrypt_payload(&key, C2S, b"sid8bytes", 7, b"h", &pt).unwrap();
        assert_eq!(ct1, ct2); // 确定性（同 nonce）→ 接收端滑动窗口可幂等拒绝
    }
}
