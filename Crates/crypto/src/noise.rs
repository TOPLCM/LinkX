//! Noise XX 握手（`Noise_XX_25519_ChaChaPoly_SHA256`，snow crate）：
//! - msg1 = e_i（明文 ephemeral 公钥）
//! - msg2 = e_r || es_r（responder 长期公钥在加密段）
//! - msg3 = s_i || se_i（initiator 长期公钥在加密段）
//!
//! msg3 用 snow 原生的 s || se 输出、未附加 ee 段——双方静态公钥已进入握手哈希与 SAS，
//! 等价安全性。PAIR_DONE 不发「长期私钥签名」（X25519 无签名语义），改为用派生会话
//! 密钥加密传输密钥占有证明。

use std::str::FromStr;

use debuglog::Level;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};
use thiserror::Error;

use crate::noise_resolver::LinkxCryptoResolver;

pub const NOISE_KEY_LEN: usize = 32;
pub const NOISE_PROTOCOL: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NoiseError {
    #[error("握手参数解析失败: {0}")]
    Init(String),
    #[error("握手消息处理失败: {0}（可能角色对调、输入被篡改或时序错误）")]
    Process(String),
    #[error("握手尚未完成或越序调用")]
    NotComplete,
    #[error("远程静态公钥尚未交换")]
    NoRemoteStatic,
    #[error("{field} 长度异常：{len}B（期望 {expected}B）")]
    UnexpectedLength {
        field: &'static str,
        len: usize,
        expected: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

/// Noise XX HandshakeState 封装（一次握手生命周期内单线程使用）
pub struct NoiseXxHandshake {
    hs: HandshakeState,
    role: Role,
    written: u8,
    read: u8,
}

impl NoiseXxHandshake {
    /// 以指定长期身份私钥建立握手（TOFU 要求长期身份可持久化复用）
    pub fn new(
        role: Role,
        local_static_sk: Option<&[u8; NOISE_KEY_LEN]>,
    ) -> Result<Self, NoiseError> {
        let params = NoiseParams::from_str(NOISE_PROTOCOL)
            .map_err(|e| NoiseError::Init(format!("{e:?}")))?;
        // snow 默认 resolver 含 AES-GCM（项目禁用一切 AES），改用自定义白名单 resolver
        let mut builder: Builder<'_> =
            Builder::with_resolver(params, Box::new(LinkxCryptoResolver));
        if let Some(sk) = local_static_sk {
            builder = builder.local_private_key(sk);
        }
        let hs = match role {
            Role::Initiator => builder.build_initiator(),
            Role::Responder => builder.build_responder(),
        }
        .map_err(|e| NoiseError::Init(format!("{e:?}")))?;
        Ok(Self {
            hs,
            role,
            written: 0,
            read: 0,
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// 角色名（日志用；不泄漏密钥材料）
    fn role_name(&self) -> &'static str {
        match self.role {
            Role::Initiator => "initiator",
            Role::Responder => "responder",
        }
    }

    pub fn is_initiator(&self) -> bool {
        self.role == Role::Initiator
    }

    /// 标准 XX 完成判定：initiator 写完 msg3、responder 读完 msg3 即完成
    pub fn is_done(&self) -> bool {
        match self.role {
            Role::Initiator => self.written == 2 && self.read == 1,
            Role::Responder => self.read == 2 && self.written == 1,
        }
    }

    /// 预写握手消息（payload 通常为空）
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.is_done() {
            return Err(NoiseError::NotComplete);
        }
        let mut out = vec![0u8; payload.len() + 256]; // e/es/se/ee 段上限充足
        let n = self
            .hs
            .write_message(payload, &mut out)
            .map_err(|e| NoiseError::Process(format!("{e:?}")))?;
        out.truncate(n);
        self.written += 1;
        // 埋点：握手写出段（msg1/2/3），只记序号/长度，不记消息字节
        debuglog::log!(
            Level::Info,
            "crypto",
            "noise.write_msg",
            &[
                ("role", self.role_name()),
                ("msg_no", &self.written.to_string()),
                ("len", &n.to_string()),
            ]
        );
        Ok(out)
    }

    /// 读入对方握手消息，返回 payload（通常为空）
    pub fn read_message(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; msg.len() + 16];
        let n = self
            .hs
            .read_message(msg, &mut buf)
            .map_err(|e| NoiseError::Process(format!("{e:?}")))?;
        buf.truncate(n);
        self.read += 1;
        debuglog::log!(
            Level::Info,
            "crypto",
            "noise.read_msg",
            &[
                ("role", self.role_name()),
                ("msg_no", &self.read.to_string()),
                ("len", &n.to_string()),
            ]
        );
        Ok(buf)
    }

    /// 握手完成后返回握手哈希（SAS / 会话密钥均由此派生）
    pub fn handshake_hash(&self) -> Result<[u8; 32], NoiseError> {
        if !self.is_done() {
            return Err(NoiseError::NotComplete);
        }
        let h = self.hs.get_handshake_hash();
        let mut out = [0u8; 32];
        let src = h.get(..out.len()).ok_or(NoiseError::UnexpectedLength {
            field: "handshake_hash",
            len: h.len(),
            expected: out.len(),
        })?;
        out.copy_from_slice(src);
        Ok(out)
    }

    /// 对方长期身份公钥（msg2/msg3 解密后获得）
    pub fn remote_static(&self) -> Result<[u8; 32], NoiseError> {
        if !self.is_done() {
            return Err(NoiseError::NotComplete);
        }
        match self.hs.get_remote_static() {
            Some(s) => {
                let mut out = [0u8; 32];
                let src = s.get(..out.len()).ok_or(NoiseError::UnexpectedLength {
                    field: "remote_static",
                    len: s.len(),
                    expected: out.len(),
                })?;
                out.copy_from_slice(src);
                Ok(out)
            }
            None => Err(NoiseError::NoRemoteStatic),
        }
    }

    /// 推进到传输模式（仅密钥刷新用；日常消息加密走 frame 层，不经此处；消费 self）
    pub fn into_transport(self) -> Result<TransportState, NoiseError> {
        if !self.is_done() {
            return Err(NoiseError::NotComplete);
        }
        self.hs
            .into_transport_mode()
            .map_err(|e| NoiseError::Process(format!("{e:?}")))
    }

    /// 完成一次 XX 握手，返回（握手哈希，远程静态公钥）
    pub fn finish(&self) -> Result<([u8; 32], [u8; 32]), NoiseError> {
        let h = self.handshake_hash()?;
        let rk = self.remote_static()?;
        // 埋点：握手收尾（只记完成事实与角色，哈希/公钥字节不入日志）
        debuglog::log!(
            Level::Info,
            "crypto",
            "noise.handshake_done",
            &[("role", self.role_name())]
        );
        Ok((h, rk))
    }
}

/// XX 握手对测结果：
/// (initiator_hash, responder_hash, initiator_remote_static, responder_remote_static)
pub type XxPairOutcome = ([u8; 32], [u8; 32], [u8; 32], [u8; 32]);

/// 完整一次 XX 握手（内存双端对测 / session 层握手协程复用），返回 [`XxPairOutcome`]
pub fn run_xx_pair(
    i_sk: &[u8; NOISE_KEY_LEN],
    r_sk: &[u8; NOISE_KEY_LEN],
) -> Result<XxPairOutcome, NoiseError> {
    let mut i = NoiseXxHandshake::new(Role::Initiator, Some(i_sk))?;
    let mut r = NoiseXxHandshake::new(Role::Responder, Some(r_sk))?;

    let msg1 = i.write_message(&[])?; // e_i
    assert!(r.read_message(&msg1)?.is_empty());

    let msg2 = r.write_message(&[])?; // e_r || es_r
    assert!(i.read_message(&msg2)?.is_empty());

    let msg3 = i.write_message(&[])?; // s_i || se_i
    assert!(r.read_message(&msg3)?.is_empty());

    debug_assert!(i.is_done() && r.is_done());

    let ih = i.handshake_hash()?;
    let rh = r.handshake_hash()?;
    let irs = i.remote_static()?; // responder 的长期公钥
    let rrs = r.remote_static()?; // initiator 的长期公钥
    Ok((ih, rh, irs, rrs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{derive_session_key, fingerprint, sas_digits};
    use rand::RngCore;

    fn rand_sk() -> [u8; 32] {
        let mut sk = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut sk);
        sk
    }

    #[test]
    fn xx_pair_derives_shared_secrets() {
        let i_sk = rand_sk();
        let r_sk = rand_sk();
        let (ih, rh, irs, rrs) = run_xx_pair(&i_sk, &r_sk).unwrap();

        assert_eq!(ih, rh, "双方握手哈希必须一致");
        assert_eq!(derive_session_key(&ih), derive_session_key(&rh));
        assert_eq!(sas_digits(&ih), sas_digits(&rh));

        assert_eq!(fingerprint(&irs).len(), 16);
        assert_eq!(fingerprint(&rrs).len(), 16);
        assert_ne!(fingerprint(&irs), fingerprint(&rrs));
    }

    #[test]
    fn tampered_msg_fails_no_panic() {
        let mut i = NoiseXxHandshake::new(Role::Initiator, Some(&rand_sk())).unwrap();
        let mut r = NoiseXxHandshake::new(Role::Responder, Some(&rand_sk())).unwrap();
        let msg1 = i.write_message(&[]).unwrap();
        assert!(r.read_message(&msg1).is_ok());

        let mut msg2 = r.write_message(&[]).unwrap();
        let n = msg2.len() - 1;
        msg2[n] ^= 0x01; // 篡改 es 密文/tag
        assert!(
            i.read_message(&msg2).is_err(),
            "篡改后握手必须失败且不 panic"
        );
        assert!(!i.is_done());
        assert!(i.finish().is_err());
    }

    #[test]
    fn garbage_input_no_panic() {
        let mut i = NoiseXxHandshake::new(Role::Initiator, Some(&rand_sk())).unwrap();
        let mut seed = 0xCAFEBABEu32;
        for _ in 0..1_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let len = (seed % 100) as usize;
            let bytes: Vec<u8> = (0..len).map(|k| ((seed >> (k % 8)) & 0xFF) as u8).collect();
            let _ = i.read_message(&bytes);
        }
        assert!(!i.is_done());
    }

    #[test]
    fn unauthorized_write_mid_handshake_fails() {
        let mut r = NoiseXxHandshake::new(Role::Responder, Some(&rand_sk())).unwrap();
        assert!(r.write_message(&[]).is_err());
    }

    #[test]
    fn pre_completion_accessors_return_error_not_panic() {
        let i = NoiseXxHandshake::new(Role::Initiator, Some(&rand_sk())).unwrap();
        assert!(matches!(i.handshake_hash(), Err(NoiseError::NotComplete)));
        assert!(matches!(i.remote_static(), Err(NoiseError::NotComplete)));
        assert!(matches!(i.finish(), Err(NoiseError::NotComplete)));
        assert!(matches!(i.into_transport(), Err(NoiseError::NotComplete)));
    }

    #[test]
    fn transport_consumes_handshake() {
        let mut i = NoiseXxHandshake::new(Role::Initiator, Some(&rand_sk())).unwrap();
        let mut r = NoiseXxHandshake::new(Role::Responder, Some(&rand_sk())).unwrap();
        let m1 = i.write_message(&[]).unwrap();
        r.read_message(&m1).unwrap();
        let m2 = r.write_message(&[]).unwrap();
        i.read_message(&m2).unwrap();
        i.write_message(&[]).unwrap(); // initiator 完成
        assert!(!r.is_done());
        let t = i.into_transport();
        assert!(t.is_ok());
        let _ = t;
    }
}
