//! 设备身份与信任库持久化。两类密钥分工（**不要混用**）：**RSA-2048 设备身份**（`device_identity.bin`）是设备识别与信任锚，
//! 跨重启稳定（否则"重启即换指纹 → 幽灵设备"）；**X25519 Noise static**（`identity.bin`）只用于建立加密通道，不参与设备识别。
//! 落盘保护：Windows 用 **DPAPI**（仅当前用户可解）；非 Windows（host 单测 / 交叉编译检查）退化为明文占位，**仅保证编译与单测，不参与真实运行**。
//! 信任库 `trusted_peers.tsv` 每行以制表符分列 `指纹 ⇥ 名称`（公钥指纹非机密）；旧版单指纹文件在首次生成 RSA 身份时归档为 `*.bak` —— 旧 X25519
//! 指纹对新身份无意义，对端需重配一次。
//! 本模块**不依赖 Windows UI 模块**以便在 host 上直接跑单测，因此未使用告警属预期、显式放行。
#![cfg_attr(not(windows), allow(dead_code))]

use std::fs;
use std::path::{Path, PathBuf};

use linkx_crypto::identity::DeviceIdentity;
use linkx_session::engine::TrustedPeer;
// TSV 解析/序列化/更新规则收在 `linkx_session::trust`（跨端单一真源），本模块只管「文件放哪、怎么保护、何时读写」
use linkx_session::trust;

const STATIC_KEY_FILE: &str = "identity.bin";
/// 旧版明文 X25519 私钥（读到即转加密存储并删除明文）
const LEGACY_STATIC_KEY_FILE: &str = "identity.hex";
/// RSA-2048 设备身份私钥（PKCS#8 DER，DPAPI 保护）
const DEVICE_IDENTITY_FILE: &str = "device_identity.bin";
const TRUST_FILE: &str = "trusted_peers.tsv";
const LEGACY_PEER_FILE: &str = "peer.hex";
/// 迁移归档后缀（旧信任不适用于新身份，保留备查但不参与判定）
const LEGACY_ARCHIVE_SUFFIX: &str = ".pre-0.3.0.bak";

/// 数据目录（**稳定绝对路径**）：`%APPDATA%\LinkX` → `%LOCALAPPDATA%\LinkX` → 显式错误，**绝不回退到 CWD**。
/// 原先 `%APPDATA%` 缺失时静默回退相对路径 —— 程序以不同工作目录启动就会在**新目录**里生成**新私钥**，
/// 对端随即看到「新设备」并连接失败
pub(crate) fn data_dir() -> Result<PathBuf, String> {
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Ok(PathBuf::from(v).join("LinkX"));
            }
        }
    }
    Err(
        "无法确定数据目录：APPDATA 与 LOCALAPPDATA 均不可用（拒绝回退到当前目录，以免身份漂移）"
            .into(),
    )
}

pub(crate) fn try_data_dir() -> Option<PathBuf> {
    data_dir().ok()
}

pub(crate) fn debug_log_dir() -> Option<PathBuf> {
    try_data_dir().map(|d| d.join("Logs"))
}

/// 读取或生成 X25519 static 私钥（DPAPI 加密落盘）。兼容旧版明文 `identity.hex`：读到时立即迁移为加密存储并删除明文，
/// **私钥字节保持不变**，因此 Noise 通道不受影响
pub(crate) fn load_or_create_static_key() -> Result<[u8; 32], String> {
    let dir = data_dir()?;
    let enc_path = dir.join(STATIC_KEY_FILE);
    let legacy_path = dir.join(LEGACY_STATIC_KEY_FILE);

    // 1) 新格式：DPAPI 保护
    if let Some(plain) = read_protected(&enc_path) {
        if let Some(sk) = bytes_to_sk(&plain) {
            return Ok(sk);
        }
    }

    // 2) 旧格式：明文 hex → 迁移为加密存储
    if let Ok(raw) = fs::read_to_string(&legacy_path) {
        if let Some(sk) = decode_sk(&raw) {
            fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
            if write_protected(&enc_path, &sk).is_ok() {
                let _ = fs::remove_file(&legacy_path); // 明文不再保留
            }
            return Ok(sk);
        }
    }

    // 3) 首次运行：生成并加密落盘
    let sk = linkx_crypto::random_bytes::<32>();
    fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    write_protected(&enc_path, &sk).map_err(|e| format!("身份私钥加密落盘失败: {e}"))?;
    Ok(sk)
}

/// 读取或生成 **RSA-2048 设备身份**（PKCS#8 DER，DPAPI 加密落盘）。首次运行（或从旧版迁移）时生成一次，
/// 生成即把旧 X25519 单指纹信任库归档（新旧指纹不同源，必须重配一次）
pub(crate) fn load_or_create_device_identity() -> Result<Vec<u8>, String> {
    let dir = data_dir()?;
    let path = dir.join(DEVICE_IDENTITY_FILE);

    if let Some(der) = read_protected(&path) {
        // 解出来的必须是合法 PKCS#8，否则视为损坏 → 重新生成（不 panic）
        if DeviceIdentity::from_pkcs8_der(&der).is_ok() {
            return Ok(der);
        }
    }

    let id = DeviceIdentity::generate().map_err(|e| format!("RSA 身份生成失败: {e}"))?;
    let der = id
        .to_pkcs8_der()
        .map_err(|e| format!("RSA 身份序列化失败: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
    write_protected(&path, &der).map_err(|e| format!("身份私钥加密落盘失败: {e}"))?;

    // 迁移副作用：旧单指纹（X25519 口径）对新身份无意义 → 归档，避免 UI 显示错误设备
    let legacy = dir.join(LEGACY_PEER_FILE);
    if legacy.exists() {
        let _ = fs::rename(
            &legacy,
            dir.join(format!("{LEGACY_PEER_FILE}{LEGACY_ARCHIVE_SUFFIX}")),
        );
    }
    Ok(der)
}

pub(crate) fn identity_fingerprint(der: &[u8]) -> Option<String> {
    DeviceIdentity::from_pkcs8_der(der).ok()?.fingerprint().ok()
}

/// 读取信任库（文件缺失/损坏一律空表，非法行跳过 —— 持久化坏了绝不能影响主流程）
pub(crate) fn load_trusted_peers() -> Vec<TrustedPeer> {
    let Some(dir) = try_data_dir() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(dir.join(TRUST_FILE)) else {
        return Vec::new();
    };
    parse_trust_lines(&raw)
}

pub(crate) fn save_trusted_peers(peers: &[TrustedPeer]) {
    let Some(dir) = try_data_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(TRUST_FILE), serialize_trust_lines(peers));
}

pub(crate) fn upsert_trusted_peer(peers: &mut Vec<TrustedPeer>, fingerprint: &str, name: &str) {
    trust::upsert(peers, fingerprint, name);
}

pub(crate) fn clear_trust_store() {
    let Some(dir) = try_data_dir() else {
        return;
    };
    let _ = fs::remove_file(dir.join(TRUST_FILE));
    let _ = fs::remove_file(dir.join(LEGACY_PEER_FILE));
    // 解绑要连"该拨谁"的名字对照一起清掉，否则下次冷启动还会去拨已经解绑的设备
    let _ = fs::remove_file(dir.join(PEER_NAME_FILE));
}

/// `peer_names.tsv`：每行以制表符分列 `指纹 ⇥ BLE 扫描名`。为什么不直接用信任库里的名字去匹配扫描列表：信任库存的是握手 TLV 的机型名
/// （如 `22041216C`），而设备列表里那条是系统蓝牙的广播友好名（`Redmi Note 11T Pro`）—— 拿前者去列表里找永远找不到，
/// 按名重连就变成空转。这份文件**只是"该拨谁"的名字对照，不是信任来源**：能不能放行仍只由 `trusted_peers.tsv` 的 RSA 指纹决定
const PEER_NAME_FILE: &str = "peer_names.tsv";

/// 线索条数上限：引擎是单对端语义，8 条足够覆盖"换过手机/重装过系统"的历史，又挡住一份被手工改大的文件把每轮匹配拖长
const PEER_NAME_MAX: usize = 8;

pub(crate) fn load_peer_names() -> Vec<(String, String)> {
    let Some(dir) = try_data_dir() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(dir.join(PEER_NAME_FILE)) else {
        return Vec::new();
    };
    parse_hint_lines(&raw)
}

pub(crate) fn save_peer_name(fingerprint: &str, scan_name: &str) {
    let name = scan_name.trim();
    let fp = fingerprint.trim();
    if name.is_empty() || fp.is_empty() {
        return;
    }
    let Some(dir) = try_data_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut hints = load_peer_names();
    hints.retain(|(f, _)| f != fp);
    hints.push((fp.to_string(), name.to_string()));
    if hints.len() > PEER_NAME_MAX {
        hints.drain(..hints.len() - PEER_NAME_MAX);
    }
    let _ = fs::write(dir.join(PEER_NAME_FILE), serialize_hint_lines(&hints));
}

/// 解析线索表：只收 `指纹<TAB>名字` 两列且都非空的行，忽略空行/注释/坏行
fn parse_hint_lines(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((fp, name)) = line.split_once('\t') else {
            continue;
        };
        let (fp, name) = (fp.trim(), name.trim());
        if fp.is_empty() || name.is_empty() {
            continue;
        }
        out.push((fp.to_string(), name.to_string()));
    }
    out
}

fn serialize_hint_lines(hints: &[(String, String)]) -> String {
    let mut s = String::new();
    for (fp, name) in hints.iter().take(PEER_NAME_MAX) {
        // 名字里带制表符/换行会把一行劈成两行：写入前一律去掉（两头都干净才不会"改了设备名，线索表凭空多出一条"）
        let clean: String = name
            .chars()
            .filter(|c| *c != '\t' && *c != '\r' && *c != '\n')
            .collect();
        s.push_str(&format!("{fp}\t{clean}\n"));
    }
    s
}

fn serialize_trust_lines(peers: &[TrustedPeer]) -> String {
    trust::to_tsv(peers)
}

fn parse_trust_lines(raw: &str) -> Vec<TrustedPeer> {
    trust::parse_tsv(raw)
}

fn decode_sk(raw: &str) -> Option<[u8; 32]> {
    let bytes = from_hex(raw.trim())?;
    bytes_to_sk(&bytes)
}

fn bytes_to_sk(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() != 32 {
        return None;
    }
    let mut sk = [0u8; 32];
    sk.copy_from_slice(bytes);
    Some(sk)
}

/// hex → 字节（长度奇数或非法字符则 None）；仅用于兼容读取旧版明文 `identity.hex`
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < raw.len() {
        let hi = (raw[i] as char).to_digit(16)?;
        let lo = (raw[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

/// 写入受保护字节（Windows：DPAPI 加密；非 Windows：明文占位）
fn write_protected(path: &Path, plain: &[u8]) -> Result<(), String> {
    let blob = protect_bytes(plain)?;
    fs::write(path, blob).map_err(|e| e.to_string())
}

fn read_protected(path: &Path) -> Option<Vec<u8>> {
    let blob = fs::read(path).ok()?;
    unprotect_bytes(&blob)
}

/// DPAPI 加密（用户级，仅当前用户可解）
#[cfg(windows)]
fn protect_bytes(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            windows::core::w!("LinkX identity"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        // "调用成功"不等于给了有效缓冲：指针为空还去切片就是未定义行为
        if out.pbData.is_null() || out.cbData == 0 {
            return Err("DPAPI 加密返回了空缓冲".to_string());
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        // DPAPI 分配的缓冲须用 LocalFree 归还
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        Ok(v)
    }
}

#[cfg(windows)]
fn unprotect_bytes(blob: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if blob.is_empty() {
        return None;
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .ok()?;
        if out.pbData.is_null() || out.cbData == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        Some(v)
    }
}

/// 非 Windows（host 单测 / 交叉编译检查）：明文占位，仅保证编译与单测可跑
#[cfg(not(windows))]
fn protect_bytes(plain: &[u8]) -> Result<Vec<u8>, String> {
    Ok(plain.to_vec())
}

#[cfg(not(windows))]
fn unprotect_bytes(blob: &[u8]) -> Option<Vec<u8>> {
    Some(blob.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `data_dir` 必须是绝对路径且**绝不**退化为 CWD 相对路径
    #[test]
    fn data_dir_is_absolute_and_never_cwd() {
        match data_dir() {
            Ok(p) => assert!(p.is_absolute(), "数据目录必须是绝对路径: {p:?}"),
            Err(_) => {
                // 环境确实没有 APPDATA/LOCALAPPDATA：允许报错，但绝不允许相对路径
                assert!(data_dir().is_err());
            }
        }
    }

    #[test]
    fn trust_lines_roundtrip() {
        let peers = vec![
            TrustedPeer {
                fingerprint: "3fa76f6244743c27".into(),
                name: "Pixel 8 Pro".into(),
            },
            TrustedPeer {
                fingerprint: "b091d4e77a2c10ff".into(),
                name: "小米\t14".into(),
            },
        ];
        let back = parse_trust_lines(&serialize_trust_lines(&peers));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].fingerprint, "3fa76f6244743c27"); // 排序后仍在首位
        assert_eq!(back[1].name, "小米 14");
    }

    #[test]
    fn legacy_hex_identity_parsing() {
        let hex = "00".repeat(32);
        assert!(decode_sk(&hex).is_some());
        assert!(decode_sk(&hex[..62]).is_none(), "长度不足必须拒绝");
        assert!(decode_sk("zz").is_none(), "非法字符必须拒绝");
    }

    #[test]
    fn device_identity_fingerprint_is_stable_16hex() {
        use std::sync::OnceLock;
        static CACHED: OnceLock<Vec<u8>> = OnceLock::new();
        let der = CACHED.get_or_init(|| {
            DeviceIdentity::generate()
                .expect("RSA 生成失败")
                .to_pkcs8_der()
                .unwrap()
        });
        let fp1 = identity_fingerprint(der).expect("指纹计算失败");
        let fp2 = identity_fingerprint(der).expect("指纹计算失败");
        assert_eq!(fp1, fp2, "同一密钥的指纹必须稳定");
        assert_eq!(fp1.len(), linkx_session::FINGERPRINT_HEX_LEN);
        assert!(fp1.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(identity_fingerprint(b"not-der").is_none());
    }

    /// 名字对照表：坏行跳过 + 名字里的制表符/换行写入前清掉（一行一键的格式，值里带换行就会凭空多出一条配置）
    #[test]
    fn peer_name_lines_survive_roundtrip_and_reject_injection() {
        let hints = vec![
            (
                "3fa76f6244743c27".to_string(),
                "Redmi Note 11T Pro".to_string(),
            ),
            (
                "b091d4e77a2c10ff".to_string(),
                "小米\t14\n伪造行=开的".to_string(),
            ),
        ];
        let text = serialize_hint_lines(&hints);
        let back = parse_hint_lines(&text);
        assert_eq!(back.len(), 2, "两行都必须活着");
        assert_eq!(back[0].1, "Redmi Note 11T Pro");
        assert_eq!(
            back[1].1, "小米14伪造行=开的",
            "制表符/换行必须被清掉，且不能劈出第三行"
        );

        // 空行、注释、缺列、空列一律跳过（持久化坏了绝不能影响主流程）
        let junk = "# comment\n\n\t名字没指纹\n指纹没名字\nno-tab-line\nx\t\nfp\tname\n";
        assert_eq!(parse_hint_lines(junk).len(), 1);
    }

    /// 非 Windows 占位保护也要能往返（保证 host 单测覆盖读写路径）
    #[cfg(not(windows))]
    #[test]
    fn protected_bytes_roundtrip_on_host() {
        let blob = protect_bytes(b"hello").unwrap();
        assert_eq!(unprotect_bytes(&blob).unwrap(), b"hello");
        // host 占位实现不加密：空输入原样返回空
        assert_eq!(unprotect_bytes(b"").unwrap(), Vec::<u8>::new());
    }
}
