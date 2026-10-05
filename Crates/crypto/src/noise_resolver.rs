//! 自定义 Noise CryptoResolver：仅 ChaCha20-Poly1305 / SHA-256 / X25519，零 AES 依赖。
//! snow 0.9.6 的 `default-resolver` feature 会拉入 `aes-gcm`（项目禁用一切 AES，含间接
//! 依赖），且 `ring-accelerated`/`libsodium-accelerated` 也连带它；故关闭 snow 默认
//! features，用 `Builder::with_resolver` + 本 resolver 只解析白名单原语。
//!
//! 原语映射（与 snow 默认实现同算子、同 nonce 编码，跨端兼容）：
//! - Cipher：ChaCha20Poly1305（12B nonce = 前 4B 置零 + `u64.to_be_bytes()`）
//! - Hash：SHA-256（sha2 crate）；Dh：X25519（x25519-dalek）；RNG：OsRng

use chacha20poly1305::aead::generic_array::{
    typenum::{U12, U16},
    GenericArray,
};
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use rand::rngs::OsRng;
use rand::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use snow::error::Error as SnowError;
use snow::params::{CipherChoice, DHChoice, HashChoice};
use snow::resolvers::CryptoResolver;
use snow::types::{Cipher, Dh, Hash, Random};
use x25519_dalek::{PublicKey, StaticSecret};

/// X25519 上阶整除 8 的 u 坐标（含 p、p+1、p-1 这些"非常规表示"）。任何私钥乘上去都会把
/// 共享密钥塌进 ≤8 个可预测值，等于**对端替双方决定会话密钥**；而 Noise XX 之后两端的握手哈希
/// 与 SAS 仍然一致，六位比对码不会报警 ⇒ 中间人可以把自己的 RSA 身份绑成"已配对设备"。
///
/// RFC 7748 §6.1 只强制"全零输出"那一档（`dh()` 里 `zero_shared` 已挡），剩下这些**输出非零**
/// 的小阶点必须逐条拒。表与 libsodium `crypto_scalarmult/curve25519/ref10/x25519_ref10.c`
/// 的 `has_small_order` 同源（7 条；X25519 协议上忽略 u 的最高位，故比较前先抹掉它），
/// 依据见 https://eprint.iacr.org/2017/806.pdf
const SMALL_ORDER_U: [[u8; 32]; 7] = [
    [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ],
    [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ],
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0x00,
    ],
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0x57,
    ],
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    [
        0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
];

/// 对端送的 u 坐标是不是小阶点（最高位按协议忽略，比较前统一抹掉）
fn is_small_order(u: &[u8; 32]) -> bool {
    let mut probe = *u;
    probe[31] &= 0x7f;
    SMALL_ORDER_U
        .iter()
        .any(|bad| probe[..31] == bad[..31] && probe[31] == (bad[31] & 0x7f))
}

/// RNG 包装（snow `Random` 需要本地新类型以满足孤儿规则）
#[derive(Default)]
struct OsRandom(OsRng);

impl RngCore for OsRandom {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.0.try_fill_bytes(dest)
    }
}

impl CryptoRng for OsRandom {}

impl Random for OsRandom {}

/// X25519 Diffie-Hellman（缓存公/私钥，满足 `Dh` 返回引用）
struct X25519Dh {
    secret: StaticSecret,
    private: [u8; 32],
    public: [u8; 32],
}

impl Default for X25519Dh {
    fn default() -> Self {
        let secret = StaticSecret::from([0u8; 32]);
        Self {
            public: PublicKey::from(&secret).to_bytes(),
            private: [0u8; 32],
            secret,
        }
    }
}

impl Dh for X25519Dh {
    fn name(&self) -> &'static str {
        "25519"
    }

    fn pub_len(&self) -> usize {
        32
    }

    fn priv_len(&self) -> usize {
        32
    }

    fn set(&mut self, privkey: &[u8]) {
        // snow 按 `priv_len()` 传 32 字节；短了说明上游契约被破坏。全 profile 是
        // `panic = "abort"`，这里 slice 越界就是把宿主进程打死，所以留痕后保持旧值
        // （握手随后自然失败），不 panic。
        let Some(bytes) = privkey.get(..32) else {
            debuglog::log!(
                debuglog::Level::Warn,
                "crypto",
                "resolver.set_short_privkey",
                &[("len", &privkey.len().to_string())]
            );
            return;
        };
        let mut k = [0u8; 32];
        k.copy_from_slice(bytes);
        self.secret = StaticSecret::from(k);
        self.private = k;
        self.public = PublicKey::from(&self.secret).to_bytes();
    }

    fn generate(&mut self, rng: &mut dyn Random) {
        let mut k = [0u8; 32];
        rng.fill_bytes(&mut k);
        self.set(&k);
    }

    fn pubkey(&self) -> &[u8] {
        &self.public
    }

    fn privkey(&self) -> &[u8] {
        &self.private
    }

    fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), SnowError> {
        // snow 传入的是整块 Buffer（cap=MAXDHLEN），真实密钥为前 pub_len 字节
        if pubkey.len() < 32 {
            debuglog::log!(
                debuglog::Level::Warn,
                "crypto",
                "dh.reject",
                &[("reason", "short_pubkey")]
            );
            return Err(SnowError::Input);
        }
        let mut their = [0u8; 32];
        their.copy_from_slice(&pubkey[..32]);
        // 小阶点（含全零）乘任何私钥都得到可预测的共享密钥 ⇒ 对端单方面决定会话密钥。
        // x25519-dalek 2.0 刻意不做这类检查，而本项目用 `default-features = false` 的 snow，
        // 本 resolver 就是唯一的 X25519 实现 ⇒ 必须在这里拒。
        if is_small_order(&their) {
            debuglog::log!(
                debuglog::Level::Warn,
                "crypto",
                "dh.reject",
                &[("reason", "small_order_pubkey")]
            );
            return Err(SnowError::Dh);
        }
        let shared = self.secret.diffie_hellman(&PublicKey::from(their));
        if shared.as_bytes() == &[0u8; 32] {
            debuglog::log!(
                debuglog::Level::Warn,
                "crypto",
                "dh.reject",
                &[("reason", "zero_shared")]
            );
            return Err(SnowError::Dh);
        }
        let n = out.len().min(shared.as_bytes().len());
        out[..n].copy_from_slice(&shared.as_bytes()[..n]);
        Ok(())
    }
}

/// SHA-256（增量 → 克隆 finalize，对应 snow `Hash` 语义）
#[derive(Default)]
struct Sha256State {
    state: Sha256,
}

impl Hash for Sha256State {
    fn name(&self) -> &'static str {
        "SHA256"
    }

    fn block_len(&self) -> usize {
        64
    }

    fn hash_len(&self) -> usize {
        32
    }

    fn reset(&mut self) {
        self.state = Sha256::new();
    }

    fn input(&mut self, data: &[u8]) {
        self.state.update(data);
    }

    fn result(&mut self, out: &mut [u8]) {
        let digest = self.state.clone().finalize();
        let n = out.len().min(digest.len());
        out[..n].copy_from_slice(&digest[..n]);
    }
}

/// ChaCha20-Poly1305 AEAD（与 snow 默认实现同 nonce 编码：
/// 12B nonce = 前 4B 置零 + u64 计数器大端；输出 = 密文 || 16B tag）
#[derive(Default)]
struct ChaChaCipher {
    key: [u8; 32],
}

impl Cipher for ChaChaCipher {
    fn name(&self) -> &'static str {
        "ChaChaPoly"
    }

    fn set(&mut self, key: &[u8]) {
        // 同上：短输入不 panic，留痕并保持旧密钥（随后的 AEAD 校验会大声失败）
        let Some(bytes) = key.get(..32) else {
            debuglog::log!(
                debuglog::Level::Warn,
                "crypto",
                "cipher.set_short_key",
                &[("len", &key.len().to_string())]
            );
            return;
        };
        self.key.copy_from_slice(bytes);
    }

    fn encrypt(&self, nonce: u64, authtext: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize {
        let aead = ChaCha20Poly1305::new(GenericArray::from_slice(&self.key));
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[4..].copy_from_slice(&nonce.to_be_bytes());
        let nonce: GenericArray<u8, U12> = nonce_bytes.into();

        out[..plaintext.len()].copy_from_slice(plaintext);
        let tag = aead
            .encrypt_in_place_detached(&nonce, authtext, &mut out[..plaintext.len()])
            .expect("ChaCha20-Poly1305 加密失败");
        out[plaintext.len()..plaintext.len() + tag.len()].copy_from_slice(&tag);
        plaintext.len() + tag.len()
    }

    fn decrypt(
        &self,
        nonce: u64,
        authtext: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, SnowError> {
        let ct_len = ciphertext.len().checked_sub(16).ok_or(SnowError::Decrypt)?;
        out[..ct_len].copy_from_slice(&ciphertext[..ct_len]);
        let aead = ChaCha20Poly1305::new(GenericArray::from_slice(&self.key));
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[4..].copy_from_slice(&nonce.to_be_bytes());
        let nonce: GenericArray<u8, U12> = nonce_bytes.into();
        let tag: GenericArray<u8, U16> = *GenericArray::from_slice(&ciphertext[ct_len..]);

        aead.decrypt_in_place_detached(&nonce, authtext, &mut out[..ct_len], &tag)
            .map_err(|_| SnowError::Decrypt)?;
        Ok(ct_len)
    }
}

/// 白名单 resolver：仅 ChaChaPoly / SHA256 / Curve25519，其余一律 None
pub struct LinkxCryptoResolver;

impl CryptoResolver for LinkxCryptoResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(OsRandom(OsRng)))
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        match choice {
            DHChoice::Curve25519 => Some(Box::new(X25519Dh::default())),
            _ => None,
        }
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        match choice {
            HashChoice::SHA256 => Some(Box::new(Sha256State::default())),
            _ => None,
        }
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        match choice {
            CipherChoice::ChaChaPoly => Some(Box::new(ChaChaCipher::default())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snow::params::NoiseParams;
    use snow::Builder;
    use std::str::FromStr;

    fn rand_sk() -> [u8; 32] {
        let mut sk = [0u8; 32];
        OsRng.fill_bytes(&mut sk);
        sk
    }

    /// 退化公钥必须被拒——否则对端可以单方面决定我们的会话密钥。
    /// 同时留一条「正常密钥必须通过」的对照，防止把检查写严到把握手全挡光。
    #[test]
    fn dh_rejects_degenerate_peer_key_but_accepts_a_real_one() {
        let mut mine = X25519Dh::default();
        mine.set(&rand_sk());
        let mut theirs = X25519Dh::default();
        theirs.set(&rand_sk());
        let mut out = [0u8; 64];

        // 小阶公钥（含全零与 p 的非常规表示）一律拒：漏一条就等于把会话密钥交给对端
        for (i, bad) in SMALL_ORDER_U.iter().enumerate() {
            let e = mine.dh(bad, &mut out);
            assert!(
                matches!(e, Err(SnowError::Dh)),
                "第 {i} 条小阶公钥必须拒绝: {e:?}"
            );
            // X25519 忽略 u 的最高位：把第 31 字节置满同一颗点，不许因此变成"可用"
            let mut msb = *bad;
            msb[31] |= 0x80;
            let e = mine.dh(&msb, &mut out);
            assert!(
                matches!(e, Err(SnowError::Dh)),
                "第 {i} 条小阶公钥的最高位变体必须同样拒绝: {e:?}"
            );
        }

        // 短于 32B 的输入不能当密钥材料（外部输入边界）
        assert!(
            matches!(mine.dh(&[1u8; 8], &mut out), Err(SnowError::Input)),
            "长度不足要按 Input 拒"
        );

        // 对照：真实握手密钥照常通过，且输出不是全零
        let peer = theirs.pubkey().to_vec();
        assert!(mine.dh(&peer, &mut out).is_ok(), "正常公钥不该被挡");
        assert_ne!(&out[..32], &[0u8; 32], "正常 DH 输出不该是全零");
        // 双方算出来的必须一致（这条保证"加了检查"没把 X25519 语义改坏）
        let mut back = [0u8; 64];
        assert!(theirs.dh(mine.pubkey(), &mut back).is_ok());
        assert_eq!(out[..32], back[..32], "共享密钥必须对称一致");
    }

    #[test]
    fn resolver_serves_whitelisted_only() {
        let resolver = LinkxCryptoResolver;
        assert!(resolver.resolve_rng().is_some());
        assert!(resolver.resolve_dh(&DHChoice::Curve25519).is_some());
        assert!(resolver.resolve_dh(&DHChoice::Ed448).is_none());
        assert!(resolver.resolve_hash(&HashChoice::SHA256).is_some());
        assert!(resolver.resolve_hash(&HashChoice::Blake2b).is_none());
        assert!(resolver.resolve_cipher(&CipherChoice::ChaChaPoly).is_some());
        assert!(resolver.resolve_cipher(&CipherChoice::AESGCM).is_none());
    }

    #[test]
    fn handshake_with_custom_resolver_pair() {
        let params = NoiseParams::from_str("Noise_XX_25519_ChaChaPoly_SHA256").unwrap();
        let mut i = Builder::with_resolver(params.clone(), Box::new(LinkxCryptoResolver))
            .local_private_key(&rand_sk())
            .build_initiator()
            .unwrap();
        let mut r = Builder::with_resolver(params.clone(), Box::new(LinkxCryptoResolver))
            .local_private_key(&rand_sk())
            .build_responder()
            .unwrap();

        let mut buf = vec![0u8; 256];
        let mut msg1 = vec![0u8; 256];
        let n = i.write_message(&[], &mut msg1).unwrap();
        msg1.truncate(n);
        r.read_message(&msg1, &mut buf).unwrap();

        let mut msg2 = vec![0u8; 256];
        let n = r.write_message(&[], &mut msg2).unwrap();
        msg2.truncate(n);
        i.read_message(&msg2, &mut buf).unwrap();

        let n = i.write_message(&[], &mut buf).unwrap(); // msg3 = s_i || se_i
        buf.truncate(n);
        r.read_message(&buf, &mut msg2).unwrap();
        assert_eq!(i.get_handshake_hash(), r.get_handshake_hash());
    }
}
