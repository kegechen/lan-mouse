//! 线协议：manifest 清单、控制帧类型、数据连接握手（HMAC-SHA256）。
//!
//! 纯逻辑跨平台可单测。控制帧沿用现有 `[u8 type][u32 BE len][payload]`；
//! 数据连接握手是独立定长报文（见 `DataReq::encode`）。

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type HmacSha256 = Hmac<Sha256>;

// ===========================================================================
// Manifest（FILE_OFFER 清单）
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub file_id: u32,
    pub relpath: String, // 正斜杠；Windows 侧转反斜杠
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub session_id: u32,
    pub entries: Vec<Entry>,
}

/// 单个 FILE_OFFER 清单的条目数上限：**sender 遍历上限 = receiver 校验上限**，
/// 两侧共用此常量对齐（见 `file::manifest::build_manifest` 与 `Manifest::validate`）。
pub const MAX_MANIFEST_ENTRIES: usize = 65_536;

/// FILE_OFFER 帧序列化字节上限，留在 16MB 帧上限之下（见 `read_raw_frame` 的 16MB）。
/// sender 发 offer 前据此守卫，避免发一个 receiver 必拒的超大帧 → 静默重连循环。
pub const MAX_OFFER_BYTES: usize = 15 * 1024 * 1024;

impl Manifest {
    pub fn to_json_bytes(&self) -> Vec<u8> {
        // 本结构 serde 序列化不会失败（无自定义 Serializer、无 IO），expect 仅表意。
        serde_json::to_vec(self).expect("serialize manifest")
    }
    pub fn from_json_bytes(b: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(b).map_err(|e| format!("bad manifest json: {e}"))
    }

    /// 校验一个（收到的）offer 是否安全可用。控制通道当前无认证，恶意/MITM 可注入
    /// 伪造 offer——粘贴时 `cFileName` 是投放目录下的相对路径，故必须挡住路径穿越
    /// 与超量条目。返回 `Err` 时整个 offer 应被丢弃（fail-safe）。
    pub fn validate(&self) -> Result<(), String> {
        if self.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(format!("too many entries: {}", self.entries.len()));
        }
        for e in &self.entries {
            if !is_safe_relpath(&e.relpath) {
                return Err(format!("unsafe relpath: {:?}", e.relpath));
            }
        }
        Ok(())
    }
}

/// relpath 安全性：只允许「投放目录下的相对路径」。拒绝空串、绝对路径、盘符
/// (`X:`)、UNC/前导分隔符、任何 `..` 段、空段（连续/结尾分隔符）、控制字符。
/// 分隔符按 `/` 与 `\` 都检查（Windows 侧会把 `/` 转成 `\`）。
pub fn is_safe_relpath(rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let b = rel.as_bytes();
    // 绝对路径 / UNC / 前导分隔符
    if b[0] == b'/' || b[0] == b'\\' {
        return false;
    }
    // 盘符 X:
    if rel.len() >= 2 && b[1] == b':' {
        return false;
    }
    // 控制字符（含 NUL）
    if rel.chars().any(|c| c.is_control()) {
        return false;
    }
    // 逐段：拒绝 `..` 与空段（连续/结尾分隔符）
    for seg in rel.split(|c: char| c == '/' || c == '\\') {
        if seg.is_empty() || seg == ".." {
            return false;
        }
    }
    true
}

// ===========================================================================
// 控制帧类型（FILE_OFFER / FILE_REVOKE）
// ===========================================================================

/// 控制帧类型枚举。
///
/// `main.rs` 出于 match 可读性直接用字面量（0x01/0x10/0x11）分发，本枚举供
/// 协议文档 + 单测校验用，不视作死代码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Text,
    FileOffer,
    FileRevoke,
}

impl FrameKind {
    pub fn type_byte(self) -> u8 {
        match self {
            Self::Text => 0x01,
            Self::FileOffer => 0x10,
            Self::FileRevoke => 0x11,
        }
    }
    pub fn from_type(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Text),
            0x10 => Some(Self::FileOffer),
            0x11 => Some(Self::FileRevoke),
            _ => None,
        }
    }
}

// ===========================================================================
// 数据连接握手 + HMAC
// ===========================================================================

pub const DATA_MAGIC: &[u8; 4] = b"LMFD";
pub const DATA_VER: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataReq {
    pub session_id: u32,
    pub file_id: u32,
    pub offset: u64,
}

impl DataReq {
    fn signed_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.session_id.to_be_bytes());
        b[4..8].copy_from_slice(&self.file_id.to_be_bytes());
        b[8..16].copy_from_slice(&self.offset.to_be_bytes());
        b
    }
    fn hmac(&self, key: &[u8]) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
        mac.update(&self.signed_bytes());
        mac.finalize().into_bytes().into()
    }
    /// 线上：[MAGIC 4][ver 1][hmac 32][session 4][file 4][offset 8] = 53 bytes
    pub fn encode(&self, key: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(53);
        v.extend_from_slice(DATA_MAGIC);
        v.push(DATA_VER);
        v.extend_from_slice(&self.hmac(key));
        v.extend_from_slice(&self.signed_bytes());
        v
    }
    pub fn decode_and_verify(buf: &[u8], key: &[u8]) -> Result<Self, String> {
        if buf.len() != 53 {
            return Err(format!("bad handshake len {}", buf.len()));
        }
        if &buf[0..4] != DATA_MAGIC {
            return Err("bad magic".into());
        }
        if buf[4] != DATA_VER {
            return Err("bad ver".into());
        }
        let mut mac_recv = [0u8; 32];
        mac_recv.copy_from_slice(&buf[5..37]);
        let req = DataReq {
            session_id: u32::from_be_bytes(buf[37..41].try_into().unwrap()),
            file_id: u32::from_be_bytes(buf[41..45].try_into().unwrap()),
            offset: u64::from_be_bytes(buf[45..53].try_into().unwrap()),
        };
        // 常量时间比较（verify_slice 内部走常量时间）
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| "hmac key")?;
        mac.update(&req.signed_bytes());
        mac.verify_slice(&mac_recv)
            .map_err(|_| "hmac mismatch".to_string())?;
        Ok(req)
    }
}

// ===========================================================================
// 控制连接认证（双向挑战-应答，复用共享 key）
// ===========================================================================

pub const CTRL_MAGIC: &[u8; 4] = b"LMCA";
pub const CTRL_VER: u8 = 1;

/// 对一个 nonce 计算 HMAC-SHA256 应答。
fn control_response(key: &[u8], nonce: &[u8; 16]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

/// 控制连接双向认证：双方各发一个新鲜 nonce，各对**对端** nonce 回
/// `HMAC(key, nonce)`，再校验对端对**自己** nonce 的应答。nonce 新鲜即防重放
/// （无 key 无法为新 nonce 伪造应答，故 nonce 可预测也安全）。任一步不符 → `Err`。
///
/// 线上（每方各发一次）：`[MAGIC 4][ver 1][nonce 16]`，随后 `[resp 32]`。
/// 双方都「先写后读」，报文远小于 socket 缓冲，不会死锁。
///
/// **安全局限**：nonce 每连接新鲜，可防跨会话重放；同会话内被动抓包重放属
/// B 档（明文）已知局限，未做 nonce 存储去重。
pub async fn control_handshake<S>(
    stream: &mut S,
    key: &[u8],
    own_nonce: [u8; 16],
) -> std::io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::io::{Error, ErrorKind};
    // 1) 发 hello（magic + ver + own_nonce）
    let mut hello = [0u8; 21];
    hello[0..4].copy_from_slice(CTRL_MAGIC);
    hello[4] = CTRL_VER;
    hello[5..21].copy_from_slice(&own_nonce);
    stream.write_all(&hello).await?;
    stream.flush().await?;
    // 2) 读对端 hello
    let mut ph = [0u8; 21];
    stream.read_exact(&mut ph).await?;
    if &ph[0..4] != CTRL_MAGIC || ph[4] != CTRL_VER {
        return Err(Error::new(ErrorKind::InvalidData, "bad control auth hello"));
    }
    let mut peer_nonce = [0u8; 16];
    peer_nonce.copy_from_slice(&ph[5..21]);
    // 3) 对对端 nonce 回应答
    let resp = control_response(key, &peer_nonce);
    stream.write_all(&resp).await?;
    stream.flush().await?;
    // 4) 读对端对我方 nonce 的应答并常量时间校验
    let mut peer_resp = [0u8; 32];
    stream.read_exact(&mut peer_resp).await?;
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| Error::new(ErrorKind::Other, "hmac key"))?;
    mac.update(&own_nonce);
    mac.verify_slice(&peer_resp)
        .map_err(|_| Error::new(ErrorKind::PermissionDenied, "control auth failed"))?;
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            session_id: 42,
            entries: vec![
                Entry {
                    file_id: 0,
                    relpath: "d".into(),
                    size: 0,
                    is_dir: true,
                },
                Entry {
                    file_id: 1,
                    relpath: "d/a.txt".into(),
                    size: 5,
                    is_dir: false,
                },
            ],
        };
        let bytes = m.to_json_bytes();
        let back = Manifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn relpath_safety() {
        // 合法：相对路径、子目录、非 ASCII
        assert!(is_safe_relpath("a.txt"));
        assert!(is_safe_relpath("d/a.txt"));
        assert!(is_safe_relpath("目录/文件.bin"));
        // 非法：空、穿越、绝对、盘符、UNC、反斜杠穿越、连续分隔
        assert!(!is_safe_relpath(""));
        assert!(!is_safe_relpath("../x"));
        assert!(!is_safe_relpath("a/../../x"));
        assert!(!is_safe_relpath("/etc/passwd"));
        assert!(!is_safe_relpath(r"\\host\share\x"));
        assert!(!is_safe_relpath(r"C:\Windows\x"));
        assert!(!is_safe_relpath(r"a\..\b"));
        assert!(!is_safe_relpath("a//b"));
    }

    #[test]
    fn manifest_validate_rejects_unsafe() {
        let bad = Manifest {
            session_id: 1,
            entries: vec![Entry {
                file_id: 0,
                relpath: "../evil".into(),
                size: 1,
                is_dir: false,
            }],
        };
        assert!(bad.validate().is_err());
        let good = Manifest {
            session_id: 1,
            entries: vec![Entry {
                file_id: 0,
                relpath: "ok/a.txt".into(),
                size: 1,
                is_dir: false,
            }],
        };
        assert!(good.validate().is_ok());
    }

    #[test]
    fn control_frame_kind() {
        assert_eq!(FrameKind::from_type(0x01), Some(FrameKind::Text));
        assert_eq!(FrameKind::from_type(0x10), Some(FrameKind::FileOffer));
        assert_eq!(FrameKind::from_type(0x11), Some(FrameKind::FileRevoke));
        assert_eq!(FrameKind::from_type(0x99), None);
    }

    #[test]
    fn handshake_hmac_roundtrip() {
        let key = b"shared-secret";
        let req = DataReq {
            session_id: 7,
            file_id: 3,
            offset: 1024,
        };
        let bytes = req.encode(key);
        // 正确 key 校验通过
        let parsed = DataReq::decode_and_verify(&bytes, key).unwrap();
        assert_eq!(parsed, req);
        // 错误 key 校验失败
        assert!(DataReq::decode_and_verify(&bytes, b"wrong").is_err());
    }

    #[tokio::test]
    async fn control_handshake_ok_and_rejects_mismatched_key() {
        // 同 key：双向认证均通过
        let (mut a, mut b) = tokio::io::duplex(1024);
        let (ra, rb) = tokio::join!(
            control_handshake(&mut a, b"shared", [1u8; 16]),
            control_handshake(&mut b, b"shared", [2u8; 16]),
        );
        assert!(ra.is_ok() && rb.is_ok());

        // 不同 key：认证失败（实际双方都失败）
        let (mut c, mut d) = tokio::io::duplex(1024);
        let (rc, rd) = tokio::join!(
            control_handshake(&mut c, b"shared", [3u8; 16]),
            control_handshake(&mut d, b"other", [4u8; 16]),
        );
        assert!(rc.is_err() || rd.is_err());
    }
}
