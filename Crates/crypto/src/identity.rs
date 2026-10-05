//! RSA-2048 持久化设备身份。设计口径：
//! - **身份载体**：RSA-2048 长期密钥对，首次启动生成一次，由平台层加密持久化
//!   （Windows DPAPI / Android Keystore AES-GCM），**绝不**明文落盘。
//! - **稳定指纹**：`SHA-256(SPKI DER 公钥)[..8]` → 16 位小写 hex；跨重启/掉线重连不变，
//!   同一物理设备始终同一指纹。
//! - **绑定证明**：Noise 握手完成后，双方经加密通道交换 `IDENTITY = pk_der || sig`，
//!   `sig = RSA-SHA256("linkx/v1/identity-bind" || hash_h)`——签名覆盖握手哈希，把
//!   「持久身份」绑定到「本次活通道」，中间人无法用旧公钥替换。
//! - **边界**：RSA 只做身份与信任判定；消息加密仍为 ChaCha20-Poly1305，握手仍走 X25519。

use debuglog::Level;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// 密钥长度（2048 位；RSA-2048 在 2030 年前满足通用安全强度要求）
pub const RSA_KEY_BITS: usize = 2048;
/// 公钥指数钉死值：`RsaPrivateKey::new` 的默认指数，也是 `verify_binding` 唯一接受的指数
pub const RSA_PUBLIC_EXPONENT: u32 = 65537;
pub const FINGERPRINT_HEX_LEN: usize = 16;
/// 身份绑定签名的域分隔前缀（防跨上下文签名重用）
pub const IDENTITY_BIND_LABEL: &[u8] = b"linkx/v1/identity-bind";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("RSA 密钥生成失败: {0}")]
    KeyGen(String),
    #[error("私钥解析失败（PKCS#8 DER）")]
    BadPrivateKey,
    #[error("公钥解析失败（SPKI DER）")]
    BadPublicKey,
    #[error("密钥序列化失败: {0}")]
    Encode(String),
    #[error("签名失败: {0}")]
    Sign(String),
}

/// 本机持久化设备身份（RSA-2048）。私钥唯一出口是 [`DeviceIdentity::to_pkcs8_der`]（平台层加密后落盘）；不实现 `Debug`/`Clone`，避免私钥泄漏到日志。
pub struct DeviceIdentity {
    sk: RsaPrivateKey,
}

impl DeviceIdentity {
    pub fn generate() -> Result<Self, IdentityError> {
        let sk = RsaPrivateKey::new(&mut rand_core::OsRng, RSA_KEY_BITS)
            .map_err(|e| IdentityError::KeyGen(e.to_string()))?;
        Ok(Self { sk })
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, IdentityError> {
        let sk = RsaPrivateKey::from_pkcs8_der(der).map_err(|_| IdentityError::BadPrivateKey)?;
        Ok(Self { sk })
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, IdentityError> {
        self.sk
            .to_pkcs8_der()
            .map(|d| d.as_bytes().to_vec())
            .map_err(|e| IdentityError::Encode(e.to_string()))
    }

    pub fn public_der(&self) -> Result<Vec<u8>, IdentityError> {
        RsaPublicKey::from(&self.sk)
            .to_public_key_der()
            .map(|d| d.as_bytes().to_vec())
            .map_err(|e| IdentityError::Encode(e.to_string()))
    }

    pub fn fingerprint(&self) -> Result<String, IdentityError> {
        self.public_der().map(|d| fingerprint_of_public_der(&d))
    }

    /// 对握手哈希做身份绑定签名（PKCS#1 v1.5 + SHA-256 + 域分隔）
    pub fn sign_binding(&self, handshake_hash: &[u8; 32]) -> Result<Vec<u8>, IdentityError> {
        let key = SigningKey::<Sha256>::new(self.sk.clone());
        let msg = binding_message(handshake_hash);
        let sig: Signature = key
            .try_sign(&msg)
            .map_err(|e| IdentityError::Sign(e.to_string()))?;
        let sig = sig.to_vec();
        debuglog::log!(
            Level::Info,
            "crypto",
            "identity.sign",
            &[("ok", "true"), ("sig_len", &sig.len().to_string())]
        );
        Ok(sig)
    }
}

/// 域分隔后的待签消息：`"linkx/v1/identity-bind" || hash_h`
fn binding_message(handshake_hash: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(IDENTITY_BIND_LABEL.len() + 32);
    msg.extend_from_slice(IDENTITY_BIND_LABEL);
    msg.extend_from_slice(handshake_hash);
    msg
}

/// 对端身份指纹：`SHA-256(SPKI DER)[..8]` → 16 位小写 hex
pub fn fingerprint_of_public_der(pk_der: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(pk_der);
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// 验证对端身份绑定签名：`pk_der` 必须能验通 `sig`（覆盖 `hash_h`）
pub fn verify_binding(pk_der: &[u8], handshake_hash: &[u8; 32], sig: &[u8]) -> bool {
    // 本机身份固定 2048 位，对端也必须如此：更短的钥匙签名可离线伪造（「指纹即信任锚」当场作废），
    // 更长的（DER 长度字段允许到 64 KB 模）每验一次就是一次大数运算，等于白送的 CPU DoS。
    let want_sig = RSA_KEY_BITS / 8;
    if sig.len() != want_sig {
        debuglog::log!(
            Level::Warn,
            "crypto",
            "identity.verify",
            &[("ok", "false"), ("reason", "bad_sig_len")]
        );
        return false;
    }
    let Ok(pk) = RsaPublicKey::from_public_key_der(pk_der) else {
        debuglog::log!(
            Level::Warn,
            "crypto",
            "identity.verify",
            &[("ok", "false"), ("reason", "bad_public_key")]
        );
        return false;
    };
    // `size()` 是字节数，模长要看 `n().bits()`——写错会把合法身份全部拒掉
    if pk.n().bits() != RSA_KEY_BITS {
        debuglog::log!(
            Level::Warn,
            "crypto",
            "identity.verify",
            &[
                ("ok", "false"),
                ("reason", "bad_key_bits"),
                ("bits", &pk.n().bits().to_string()),
            ]
        );
        return false;
    }
    // 指数同样要钉死：`rsa` 只保证 e 是奇数且落在 [3, 2^33-1]，而信任锚恰恰来自**对端自送的
    // DER**——不查就是让对端自己挑一个便于伪造的指数（PKCS#1 v1.5 配 e=3 这类小指数可离线造签：
    // 2048 位模数下 e=3 的立方根不会溢出，Coppersmith 一档直接开出来）。本机密钥恒为 65537。
    if pk.e() != &rsa::BigUint::from(RSA_PUBLIC_EXPONENT) {
        debuglog::log!(
            Level::Warn,
            "crypto",
            "identity.verify",
            &[("ok", "false"), ("reason", "bad_public_exponent")]
        );
        return false;
    }
    let Ok(sig) = Signature::try_from(sig) else {
        debuglog::log!(
            Level::Warn,
            "crypto",
            "identity.verify",
            &[("ok", "false"), ("reason", "bad_sig")]
        );
        return false;
    };
    let key = VerifyingKey::<Sha256>::new(pk);
    let ok = key.verify(&binding_message(handshake_hash), &sig).is_ok();
    debuglog::log!(
        Level::Info,
        "crypto",
        "identity.verify",
        &[
            ("ok", if ok { "true" } else { "false" }),
            ("fp", &fingerprint_of_public_der(pk_der)),
        ]
    );
    ok
}

// ---- IDENTITY 消息载荷编解码（长度前缀二进制；TLV 1B len 上限装不下 294B SPKI） ----

/// 编码：`u16 pk_len | pk_der | u16 sig_len | sig`（均大端）。装不进 u16 长度前缀时返回 `None`
/// 而不是静默截断成另一个数（双端会解出不同载荷）。
pub fn encode_identity_payload(pk_der: &[u8], sig: &[u8]) -> Option<Vec<u8>> {
    let (Ok(pk_len), Ok(sig_len)) = (u16::try_from(pk_der.len()), u16::try_from(sig.len())) else {
        return None;
    };
    let mut out = Vec::with_capacity(4 + pk_der.len() + sig.len());
    out.extend_from_slice(&pk_len.to_be_bytes());
    out.extend_from_slice(pk_der);
    out.extend_from_slice(&sig_len.to_be_bytes());
    out.extend_from_slice(sig);
    Some(out)
}

pub fn decode_identity_payload(payload: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if payload.len() < 4 {
        return None;
    }
    let pk_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + pk_len + 2 {
        return None;
    }
    let pk = payload[2..2 + pk_len].to_vec();
    let rest = &payload[2 + pk_len..];
    let sig_len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
    if rest.len() < 2 + sig_len {
        return None;
    }
    let sig = rest[2..2 + sig_len].to_vec();
    Some((pk, sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> DeviceIdentity {
        // RSA 生成较慢（~100ms 级），OnceLock 缓存一次供全部用例共享
        use std::sync::OnceLock;
        static CACHED: OnceLock<Vec<u8>> = OnceLock::new();
        let der =
            CACHED.get_or_init(|| DeviceIdentity::generate().unwrap().to_pkcs8_der().unwrap());
        DeviceIdentity::from_pkcs8_der(der).unwrap()
    }

    #[test]
    fn identity_roundtrip_and_stable_fingerprint() {
        let id = ident();
        let der = id.to_pkcs8_der().unwrap();
        let id2 = DeviceIdentity::from_pkcs8_der(&der).unwrap();
        assert_eq!(id.fingerprint().unwrap(), id2.fingerprint().unwrap());
        assert_eq!(id.fingerprint().unwrap().len(), FINGERPRINT_HEX_LEN);
        assert!(id
            .fingerprint()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn identity_fingerprint_differs_across_keys() {
        let a = ident();
        let b = DeviceIdentity::generate().unwrap();
        assert_ne!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
    }

    #[test]
    fn binding_sign_verify_roundtrip() {
        let id = ident();
        let pk = id.public_der().unwrap();
        let hash = [0x42u8; 32];
        let sig = id.sign_binding(&hash).unwrap();
        assert!(verify_binding(&pk, &hash, &sig));
        assert!(!verify_binding(&pk, &[0x43u8; 32], &sig));
        assert!(!verify_binding(&pk, &hash, &sig[..sig.len() - 1]));
        let mut bad = sig.clone();
        bad[0] ^= 1;
        assert!(!verify_binding(&pk, &hash, &bad));
    }

    #[test]
    fn binding_rejects_wrong_key() {
        let a = ident();
        let b = DeviceIdentity::generate().unwrap();
        let hash = [0x11u8; 32];
        let sig = a.sign_binding(&hash).unwrap();
        assert!(!verify_binding(&b.public_der().unwrap(), &hash, &sig));
    }

    /// 指数钉死判据的两半：
    /// ① 本机真实密钥的指数必须就是钉住的那个值 —— 写错这条会把所有合法身份全拒掉，
    ///    而症状是"配对永远不成功"，所以它得由测试而不是由用户发现；
    /// ② 同模数换 e=3 的公钥（DER 完全合法、`rsa` 也肯解析）不许通过入口。
    ///    ②单独看是弱判据（换了指数的本来密钥也验不过），它守的是"这条分支存在且先于验签"。
    #[test]
    fn public_exponent_is_pinned() {
        use rsa::traits::PublicKeyParts;
        let id = ident();
        let der = id.public_der().unwrap();
        let pk = RsaPublicKey::from_public_key_der(&der).unwrap();
        assert_eq!(
            pk.e(),
            &rsa::BigUint::from(RSA_PUBLIC_EXPONENT),
            "本机密钥指数与钉死值不一致：verify_binding 会把所有对端拒掉"
        );
        let hash = [0x7fu8; 32];
        let sig = id.sign_binding(&hash).unwrap();
        let weird = RsaPublicKey::new(pk.n().clone(), rsa::BigUint::from(3u8)).unwrap();
        let weird_der = weird.to_public_key_der().unwrap();
        assert!(
            !verify_binding(weird_der.as_bytes(), &hash, &sig),
            "对端自送 DER 里的指数没被钉住"
        );
    }

    #[test]
    fn identity_payload_codec_roundtrip() {
        let id = ident();
        let pk = id.public_der().unwrap();
        let sig = id.sign_binding(&[0x01u8; 32]).unwrap();
        let payload = encode_identity_payload(&pk, &sig).expect("294B DER + 256B 签名装得进 u16");
        let (pk2, sig2) = decode_identity_payload(&payload).unwrap();
        assert_eq!(pk, pk2);
        assert_eq!(sig, sig2);
        // 越过长度前缀：宁可不出这条消息，也不能发出一条对端解成别的东西的载荷
        assert!(encode_identity_payload(&vec![0u8; 70000], &sig).is_none());
        assert!(encode_identity_payload(&pk, &vec![0u8; 70000]).is_none());
        assert!(decode_identity_payload(&[]).is_none());
        assert!(decode_identity_payload(&[0x00, 0x02, 0xAA]).is_none());
        assert!(decode_identity_payload(&[0xFF, 0xFF, 0x00, 0x02, 0xAA]).is_none());
    }

    #[test]
    fn fingerprint_of_public_der_matches_device() {
        let id = ident();
        let pk = id.public_der().unwrap();
        assert_eq!(fingerprint_of_public_der(&pk), id.fingerprint().unwrap());
    }
}
