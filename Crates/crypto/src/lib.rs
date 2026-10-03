//! LinkX crypto crate — 安全设计
//!
//! - 消息加密：ChaCha20-Poly1305（全文唯一加密方案，禁止 AES-GCM）
//! - nonce：`HMAC(session_key, direction || seq || session_id)`[0..12]
//! - 握手：Noise_XX_25519_ChaChaPoly_SHA256（snow crate）
//! - SAS：从握手哈希派生；设备身份：RSA-2048 持久密钥对，
//!   指纹 `SHA-256(SPKI DER)[..8]` 十六进制（见 `identity`）
//! - 抗重放：接收侧滑动窗口（见 `replay`）

pub mod binding;
pub mod cipher;
pub mod identity;
pub mod noise;
pub mod noise_resolver;
pub mod replay;

pub use identity::{
    decode_identity_payload, encode_identity_payload, fingerprint_of_public_der, verify_binding,
    DeviceIdentity, IdentityError, FINGERPRINT_HEX_LEN,
};
pub use replay::{ReplayError, ReplayWindow, REPLAY_OUT_OF_ORDER_TOLERANCE, REPLAY_WINDOW_SIZE};

use sha2::{Digest, Sha256};

/// TOFU 指纹：对方长期身份公钥 SHA-256 → 16 位十六进制
///
/// **位宽取舍**：只取摘要前 8B（64-bit）。TOFU 场景下攻击者需在碰巧命中前尝试约 2^32 次
/// （且每次都要让用户看到并确认同一指纹），概率可忽略；但它**不是**抗碰撞承诺，对外材料
/// 不得表述为「指纹不可伪造」。若后续要提升强度，改这里一处即可（两侧同步）。
pub fn fingerprint(long_term_pk: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(long_term_pk);
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// 由长期身份私钥导出长期公钥（X25519）。
/// 用途：本端指纹自检 / 对端指纹预置（`fingerprint(public_key_of(sk))`）；
/// 握手路径中的对端公钥由 Noise 自动交换，无需本函数。
pub fn public_key_of(long_term_sk: &[u8; 32]) -> [u8; 32] {
    let secret = x25519_dalek::StaticSecret::from(*long_term_sk);
    x25519_dalek::PublicKey::from(&secret).to_bytes()
}

/// SAS 派生：`HMAC(key = handshake_hash, msg = "SAS")` → 6 位十进制数字。
/// 采用前 3 字节 24bit mod 1_000_000（配对场景偏差可忽略）。
use hmac::{Hmac, Mac};

type HmacSha256 = Hmac<sha2::Sha256>;

pub fn sas_digits(handshake_hash: &[u8; 32]) -> u32 {
    // HMAC 接受任意长度 key，32B 输入数学上不会失败；用 unreachable! 显式声明不变式
    let mut mac = match <HmacSha256 as Mac>::new_from_slice(handshake_hash) {
        Ok(m) => m,
        Err(_) => unreachable!("HMAC-SHA256 接受任意长度 key，32B 输入必然成功"),
    };
    mac.update(b"SAS");
    let out = mac.finalize().into_bytes();
    let v = ((out[0] as u32) << 16) | ((out[1] as u32) << 8) | out[2] as u32;
    v % 1_000_000
}

/// 会话密钥派生：HKDF-SHA256(ikm=handshake_hash, salt="linkx/v1", info="session-key")
pub fn derive_session_key(handshake_hash: &[u8; 32]) -> [u8; 32] {
    use hkdf::Hkdf;
    let hk = Hkdf::<sha2::Sha256>::new(Some(b"linkx/v1"), handshake_hash);
    let mut okm = [0u8; 32];
    // HKDF 32B 扩张远小于 255*hash_len 上限，数学上不会失败
    if hk.expand(b"session-key", &mut okm).is_err() {
        unreachable!("HKDF-SHA256 32B 扩张必然成功（上限 255*32B）");
    }
    // 埋点：会话密钥派生（只记 ok/长度，**绝不记密钥字节**）
    debuglog::log!(
        debuglog::Level::Info,
        "crypto",
        "session_key.derive",
        &[("ok", "true"), ("len", "32")]
    );
    okm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_is_6_digits_range() {
        let hash = [0xABu8; 32];
        let sas = sas_digits(&hash);
        assert!(sas < 1_000_000);
        let sas2 = sas_digits(&[0xACu8; 32]);
        // 不同握手哈希 → 大概率不同 SAS
        assert_ne!(sas, sas2);
    }

    #[test]
    fn sas_construction_matches_spec() {
        // 显式写出构造 sas = HMAC(key = hash_h, msg = "SAS")[0..3] mod 1e6，
        // 防止 key/msg 参数顺序被再次写反。
        let hash = [0x5Au8; 32];
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&hash).unwrap();
        mac.update(b"SAS");
        let out = mac.finalize().into_bytes();
        let expect = (((out[0] as u32) << 16) | ((out[1] as u32) << 8) | out[2] as u32) % 1_000_000;
        assert_eq!(sas_digits(&hash), expect);
    }

    #[test]
    fn sas_known_answer_vector() {
        // 回归基线（KAT）：锁定 HMAC(hash_h, "SAS") 实现，防未来变更破坏跨端一致性。
        // 若此处失败，必须同步更新 Kotlin/文档端派生物，不能只改常量。
        assert_eq!(sas_digits(&[0xABu8; 32]), 50926);
        assert_eq!(sas_digits(&[0x5Au8; 32]), 889781);
    }

    #[test]
    fn fingerprint_is_16_hex() {
        let pk = [0x12u8; 32];
        let fp = fingerprint(&pk);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fingerprint(&pk), fingerprint(&pk));
        let mut pk2 = pk;
        pk2[0] ^= 1;
        assert_ne!(fp, fingerprint(&pk2));
    }

    #[test]
    fn public_key_of_matches_handshake_remote_static() {
        // 与真实 XX 握手交叉验证：responder 的 long-term 公钥 == public_key_of(其私钥)
        let sk = [0x9Bu8; 32];
        let (_ih, _rh, irs, _rrs) = noise::run_xx_pair(&[0x11u8; 32], &sk).unwrap();
        assert_eq!(irs, public_key_of(&sk));
        assert_eq!(public_key_of(&sk), public_key_of(&sk)); // 稳定可复现（TOFU 预置指纹依赖此性质）
        assert_ne!(public_key_of(&sk), public_key_of(&[0x9Cu8; 32]));
    }

    #[test]
    fn session_key_derives() {
        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];
        let k1 = derive_session_key(&h1);
        let k2 = derive_session_key(&h1);
        let k3 = derive_session_key(&h2);
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_eq!(k1.len(), 32);
    }
}
