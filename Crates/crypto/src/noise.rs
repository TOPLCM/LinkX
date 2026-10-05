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

    /// 取 Noise `Split(ck)` 的两把传输密钥 `(initiator→responder, responder→initiator)`，
    /// 会话密钥由 [`crate::derive_session_key`] 从它派生。
    ///
    /// 为什么必须走这里而不是直接用 [`Self::handshake_hash`]：`h` 是线路字节的链式哈希，
    /// 谁录到三次握手都能自己重算一遍 —— 它证明"双方看到的是同一次握手"，不证明任何秘密。
    /// `ck` 才吃进了 es/se 两次 X25519 DH。
    ///
    /// 只在握手完成后调用一次：`Split` 会重置内部链式密钥，之后这条握手状态不再参与加解密。
    pub fn transport_split(
        &mut self,
    ) -> Result<([u8; NOISE_KEY_LEN], [u8; NOISE_KEY_LEN]), NoiseError> {
        if !self.is_done() {
            return Err(NoiseError::NotComplete);
        }
        let (k1, k2) = self.hs.dangerously_get_raw_split();
        debuglog::log!(
            Level::Info,
            "crypto",
            "noise.split",
            &[("role", self.role_name())]
        );
        Ok((k1, k2))
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

/// 一次完整 XX 握手的双端结果。`i_` / `r_` 前缀 = initiator / responder 视角。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XxPairOutcome {
    pub i_hash: [u8; 32],
    pub r_hash: [u8; 32],
    /// initiator 解出的对端长期公钥（即 responder 的 static）
    pub i_remote: [u8; 32],
    pub r_remote: [u8; 32],
    /// Noise `Split(ck)` 的两把传输密钥——会话密钥的 ikm。双端应当完全相等。
    pub i_split: ([u8; NOISE_KEY_LEN], [u8; NOISE_KEY_LEN]),
    pub r_split: ([u8; NOISE_KEY_LEN], [u8; NOISE_KEY_LEN]),
}

/// 完整一次 XX 握手（内存双端对测 / 自检用），返回 [`XxPairOutcome`]
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
    let i_split = i.transport_split()?;
    let r_split = r.transport_split()?;
    Ok(XxPairOutcome {
        i_hash: ih,
        r_hash: rh,
        i_remote: irs,
        r_remote: rrs,
        i_split,
        r_split,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fingerprint, random_bytes, sas_digits};

    fn rand_sk() -> [u8; 32] {
        random_bytes()
    }

    #[test]
    fn xx_pair_derives_shared_secrets() {
        let i_sk = rand_sk();
        let r_sk = rand_sk();
        let out = run_xx_pair(&i_sk, &r_sk).unwrap();

        assert_eq!(out.i_hash, out.r_hash, "双方握手哈希必须一致");
        assert_eq!(sas_digits(&out.i_hash), sas_digits(&out.r_hash));
        assert_eq!(out.i_split, out.r_split, "双方 Split 出的传输密钥必须一致");

        assert_eq!(fingerprint(&out.i_remote).len(), 16);
        assert_eq!(fingerprint(&out.r_remote).len(), 16);
        assert_ne!(
            fingerprint(&out.i_remote),
            fingerprint(&out.r_remote),
            "双方的长期身份不能相同"
        );
    }

    /// 旁观者视角：录下三次握手的全部线路字节，就能自己重算出握手哈希 `h` —— 所以 `h`
    /// 不能当密钥材料（0.5.1 之前正是这么做的，等于链路上没有秘密）；而会话密钥现在从
    /// `Split(ck)` 派生，没有 es/se 的私钥就算不出来。这条测试同时钉住「改回去就红」。
    #[test]
    fn eavesdropper_can_recompute_hash_but_not_the_session_key() {
        use sha2::{Digest, Sha256};

        /// h 只往前推：`h = SHA256(h || 线路片段)`，空片段也要过一次（协议就是这么规定的）
        fn mix(h: &[u8], part: &[u8]) -> Vec<u8> {
            let mut n = Sha256::new();
            n.update(h);
            n.update(part);
            n.finalize().to_vec()
        }

        let mut i = NoiseXxHandshake::new(Role::Initiator, Some(&rand_sk())).unwrap();
        let mut r = NoiseXxHandshake::new(Role::Responder, Some(&rand_sk())).unwrap();
        let m1 = i.write_message(&[]).unwrap();
        r.read_message(&m1).unwrap();
        let m2 = r.write_message(&[]).unwrap();
        i.read_message(&m2).unwrap();
        let m3 = i.write_message(&[]).unwrap();
        r.read_message(&m3).unwrap();

        let h_i = i.handshake_hash().unwrap();
        let h_r = r.handshake_hash().unwrap();
        assert_eq!(h_i, h_r);

        // XX 的线路形状（DH=32B 公钥，TAG=16B AEAD tag）：
        // msg1 = e_i；msg2 = e_r || 加密的 s_r || 空 payload 的 tag；msg3 = 加密的 s_i || tag
        const DH: usize = 32;
        const TAG: usize = 16;
        assert_eq!(m1.len(), DH);
        assert_eq!(m2.len(), 2 * (DH + TAG));
        assert_eq!(m3.len(), DH + 2 * TAG);

        // 逐 token 重放：E 段是明文公钥，S 段是密文+tag，空 payload 也占一个 tag，
        // 全都躺在链路上；DH 段只推进链式密钥 ck（那才是秘密），不进 h。
        let mut h = Sha256::digest(NOISE_PROTOCOL.as_bytes()).to_vec(); // 初始 h=协议名，再过一遍空 prologue
        for seg in [
            &m1[..],
            &[][..], // msg1 时尚无密钥，空 payload 不占字节
            &m2[..DH],
            &m2[DH..2 * DH + TAG],
            &m2[2 * DH + TAG..],
            &m3[..DH + TAG],
            &m3[DH + TAG..],
        ] {
            h = mix(&h, seg);
        }
        assert_eq!(
            h_i,
            <[u8; 32]>::try_from(&h[..32]).unwrap(),
            "h 只由线路字节决定：录到包就能算出来"
        );

        // 双端各自派生的会话密钥一致
        let ki = i.transport_split().unwrap();
        let kr = r.transport_split().unwrap();
        assert_eq!(ki, kr, "双方 Split 出的传输密钥必须一致");
        let key_i = crate::derive_session_key(&h_i, (&ki.0, &ki.1));
        let key_r = crate::derive_session_key(&h_r, (&kr.0, &kr.1));
        assert_eq!(key_i, key_r, "两端必须算出同一把会话密钥");

        // 拿着同一个 h、却没有 DH 秘密的人派生不出它
        assert_ne!(
            key_i,
            crate::derive_session_key(&h_i, (&[0u8; 32], &[0u8; 32]))
        );
        // 旧公式（ikm = h）也不等于新密钥：谁把派生改回去，这条就红
        use hkdf::Hkdf;
        let hk = Hkdf::<sha2::Sha256>::new(Some(b"linkx/v1"), &h_i);
        let mut old = [0u8; 32];
        hk.expand(b"session-key", &mut old).unwrap();
        assert_ne!(key_i, old, "会话密钥不得再等于「只用握手哈希」的旧派生");
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
