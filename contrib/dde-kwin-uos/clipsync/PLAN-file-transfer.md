# clipsync 文件复制粘贴 实现计划（Linux X11 → Windows）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在远端 Linux 复制文件/文件夹后，漫游回本地 Windows 在桌面 `Ctrl+V` 粘贴，文件经网络按需流式传回并落地，粘贴时显示 Explorer 原生进度框。

**Architecture:** 在现有 `clipsync` crate 内扩展。控制信令（`FILE_OFFER` 清单）走现有 clipsync TCP；文件字节走**独立数据连接**（Windows 按需连 Linux 数据端口、HMAC 认证、裸流拉取）。Windows 侧用 Win32/OLE COM 做异步 `IDataObject` + `IStream`（delayed rendering），跑在专用 STA 线程；Linux 侧用 xclip 检测 X11 `text/uri-list`、tokio `data_server` 从磁盘按 offset pump 字节。复制那刻零传输，字节全在粘贴时拉。

**Tech Stack:** Rust / tokio / serde_json / hmac+sha2 / arboard（文本，保留）/ xclip（Linux 检测）/ `windows` crate（Windows COM）。

---

## 依赖与版本约定（实现前先做）

- [ ] **Step 0.1：确认 windows crate 版本并记录到本计划**

clipsync 现有 `Cargo.lock` 经 arboard 只引入了 `windows-sys`（raw FFI），**不含**能 `#[implement]` COM 接口的高层 `windows` crate。本特性需要后者。

Run（在 `contrib/dde-kwin-uos/clipsync/`）：
```bash
cargo add windows --target 'cfg(windows)' \
  --features Win32_System_Com,Win32_System_Ole,Win32_System_DataExchange,Win32_System_Memory,Win32_System_Com_StructuredStorage,Win32_UI_WindowsAndMessaging,Win32_Foundation,Win32_System_SystemServices
cargo add hmac sha2
```
记录 `cargo add` 实际锁定的 `windows` 版本号（如 `0.58.x`）到这里：`windows = "____"`。**后续所有 COM 任务的签名核对，都以这个锁定版本的 docs.rs 为准。**

- [ ] **Step 0.2：Cargo.toml 新增数据端口/参数占位（先不接线）**

不改代码逻辑，仅确认 `Args`（`main.rs`）后续要加 `--file-data-port`（Linux）与派生的连接目标（Windows 复用 connect 主机 + 该端口）。此步只记录，不实现。

---

## 文件结构

```
clipsync/src/
  main.rs            // 现有：arg + tokio 控制循环 + 文本轮询/handle_connection；接入文件路径
  proto.rs           // 新：帧类型、manifest(serde)、数据握手编解码、HMAC 计算/校验
  clipboard_text.rs  // 新：从 main.rs 抽出的 read/write_local_clipboard（行为不变）
  file/
    mod.rs           // 新：pub mod manifest; 平台子模块 cfg 导出
    manifest.rs      // 新：递归遍历、Entry、file_id 映射、Session（跨平台纯逻辑）
    detect_x11.rs    // 新 #[cfg(unix)]：xclip TARGETS+uri-list 轮询、file:// 解析
    data_server.rs   // 新 #[cfg(unix)]：tokio 监听、握手认证、按 offset pump
    clipboard_owner.rs // 新 #[cfg(windows)]：STA 线程、OleInitialize、消息泵、OleSetClipboard
    data_object.rs     // 新 #[cfg(windows)]：FileDataObject + FILEGROUPDESCRIPTORW 构建
    net_stream.rs      // 新 #[cfg(windows)]：NetStream(IStream over 阻塞 TCP)
```

原则：`proto.rs`/`file/manifest.rs` 纯逻辑跨平台可单测；平台细节隔到各 cfg 文件。每个文件单一职责。

---

## Task 1: 抽出现有文本逻辑到 clipboard_text.rs（安全重构，行为不变）

**Files:**
- Create: `contrib/dde-kwin-uos/clipsync/src/clipboard_text.rs`
- Modify: `contrib/dde-kwin-uos/clipsync/src/main.rs`

- [ ] **Step 1.1：新建 clipboard_text.rs，原样搬迁**

把 `main.rs` 中的 `ClipboardItem`、`read_local_clipboard`、`write_local_clipboard`、`unix_try_read`、`unix_write_via`（第 58–200 行区间）整体剪切到 `clipboard_text.rs`，加 `pub`：

```rust
//! 文本剪贴板读写（从 main.rs 抽出，行为完全不变）。
use log::debug;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardItem {
    Text(String),
}

impl ClipboardItem {
    pub fn type_byte(&self) -> u8 { match self { ClipboardItem::Text(_) => 0x01 } }
    pub fn payload(&self) -> Vec<u8> { match self { ClipboardItem::Text(s) => s.as_bytes().to_vec() } }
    pub fn from_frame(type_byte: u8, payload: Vec<u8>) -> Result<Self, String> {
        match type_byte {
            0x01 => String::from_utf8(payload).map(ClipboardItem::Text)
                .map_err(|e| format!("invalid utf-8 text: {e}")),
            other => Err(format!("unknown frame type 0x{other:02x}")),
        }
    }
    pub fn hash(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        h ^= self.type_byte() as u64;
        h = h.wrapping_mul(0x100000001b3);
        for &b in self.payload().iter() { h ^= b as u64; h = h.wrapping_mul(0x100000001b3); }
        h
    }
}
// read_local_clipboard / write_local_clipboard / unix_try_read / unix_write_via 原样搬入，加 pub。
```

- [ ] **Step 1.2：main.rs 顶部声明模块并 use**

```rust
mod clipboard_text;
mod proto;
mod file;
use clipboard_text::{ClipboardItem, read_local_clipboard, write_local_clipboard};
```

- [ ] **Step 1.3：编译验证行为不变**

Run: `cargo build --release`
Expected: 编译通过，无警告新增。文本同步逻辑一行未改。

- [ ] **Step 1.4：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/clipboard_text.rs contrib/dde-kwin-uos/clipsync/src/main.rs
git commit -m "refactor(clipsync): 抽出文本剪贴板逻辑到 clipboard_text 模块"
```

---

## Task 2: proto.rs — manifest 结构 + JSON 编解码（TDD）

**Files:**
- Create: `contrib/dde-kwin-uos/clipsync/src/proto.rs`
- Test: 同文件 `#[cfg(test)] mod tests`

- [ ] **Step 2.1：写失败测试**

```rust
// proto.rs 末尾
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            session_id: 42,
            entries: vec![
                Entry { file_id: 0, relpath: "d".into(), size: 0, is_dir: true },
                Entry { file_id: 1, relpath: "d/a.txt".into(), size: 5, is_dir: false },
            ],
        };
        let bytes = m.to_json_bytes();
        let back = Manifest::from_json_bytes(&bytes).unwrap();
        assert_eq!(m, back);
    }
}
```

- [ ] **Step 2.2：运行确认失败**

Run: `cargo test --lib manifest_roundtrip`
Expected: FAIL（`Manifest` 未定义）。

- [ ] **Step 2.3：最小实现**

```rust
use serde::{Deserialize, Serialize};

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

impl Manifest {
    pub fn to_json_bytes(&self) -> Vec<u8> { serde_json::to_vec(self).expect("serialize manifest") }
    pub fn from_json_bytes(b: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(b).map_err(|e| format!("bad manifest json: {e}"))
    }
}
```

在 `Cargo.toml` 加 `serde = { version = "1", features = ["derive"] }` 和 `serde_json = "1"`（两平台）。

- [ ] **Step 2.4：运行确认通过**

Run: `cargo test --lib manifest_roundtrip`
Expected: PASS。

- [ ] **Step 2.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/proto.rs contrib/dde-kwin-uos/clipsync/Cargo.toml
git commit -m "feat(clipsync): proto manifest 结构与 JSON 编解码"
```

---

## Task 3: proto.rs — 控制帧类型（FILE_OFFER / FILE_REVOKE）（TDD）

**Files:** Modify `proto.rs`

- [ ] **Step 3.1：写失败测试**

```rust
#[test]
fn control_frame_kind() {
    assert_eq!(FrameKind::from_type(0x01), Some(FrameKind::Text));
    assert_eq!(FrameKind::from_type(0x10), Some(FrameKind::FileOffer));
    assert_eq!(FrameKind::from_type(0x11), Some(FrameKind::FileRevoke));
    assert_eq!(FrameKind::from_type(0x99), None);
}
```

- [ ] **Step 3.2：运行确认失败**

Run: `cargo test --lib control_frame_kind`
Expected: FAIL。

- [ ] **Step 3.3：最小实现**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind { Text, FileOffer, FileRevoke }
impl FrameKind {
    pub fn type_byte(self) -> u8 { match self { Self::Text => 0x01, Self::FileOffer => 0x10, Self::FileRevoke => 0x11 } }
    pub fn from_type(b: u8) -> Option<Self> {
        match b { 0x01 => Some(Self::Text), 0x10 => Some(Self::FileOffer), 0x11 => Some(Self::FileRevoke), _ => None }
    }
}
```

- [ ] **Step 3.4：运行确认通过**

Run: `cargo test --lib control_frame_kind`
Expected: PASS。

> 注：帧的线上格式沿用现有 `[u8 type][u32 BE len][payload]`（见 `main.rs` 的 `write_frame`/`read_frame`）。Task 9 会把 `write_frame`/`read_frame` 泛化成"按 type 分发"。

- [ ] **Step 3.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/proto.rs
git commit -m "feat(clipsync): proto 控制帧类型 FileOffer/FileRevoke"
```

---

## Task 4: proto.rs — 数据连接握手 + HMAC（TDD）

**Files:** Modify `proto.rs`, `Cargo.toml`

- [ ] **Step 4.1：写失败测试**

```rust
#[test]
fn handshake_hmac_roundtrip() {
    let key = b"shared-secret";
    let req = DataReq { session_id: 7, file_id: 3, offset: 1024 };
    let bytes = req.encode(key);
    // 正确 key 校验通过
    let parsed = DataReq::decode_and_verify(&bytes, key).unwrap();
    assert_eq!(parsed, req);
    // 错误 key 校验失败
    assert!(DataReq::decode_and_verify(&bytes, b"wrong").is_err());
}
```

- [ ] **Step 4.2：运行确认失败**

Run: `cargo test --lib handshake_hmac_roundtrip`
Expected: FAIL。

- [ ] **Step 4.3：最小实现**

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;
type HmacSha256 = Hmac<Sha256>;

pub const DATA_MAGIC: &[u8; 4] = b"LMFD";
pub const DATA_VER: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataReq { pub session_id: u32, pub file_id: u32, pub offset: u64 }

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
        if buf.len() != 53 { return Err(format!("bad handshake len {}", buf.len())); }
        if &buf[0..4] != DATA_MAGIC { return Err("bad magic".into()); }
        if buf[4] != DATA_VER { return Err("bad ver".into()); }
        let mut mac_recv = [0u8; 32]; mac_recv.copy_from_slice(&buf[5..37]);
        let req = DataReq {
            session_id: u32::from_be_bytes(buf[37..41].try_into().unwrap()),
            file_id: u32::from_be_bytes(buf[41..45].try_into().unwrap()),
            offset: u64::from_be_bytes(buf[45..53].try_into().unwrap()),
        };
        // 常量时间比较
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| "hmac key")?;
        mac.update(&req.signed_bytes());
        mac.verify_slice(&mac_recv).map_err(|_| "hmac mismatch".to_string())?;
        Ok(req)
    }
}
```

`Cargo.toml`（两平台）加：`hmac = "0.12"`、`sha2 = "0.10"`。

- [ ] **Step 4.4：运行确认通过**

Run: `cargo test --lib handshake_hmac_roundtrip`
Expected: PASS。

- [ ] **Step 4.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/proto.rs contrib/dde-kwin-uos/clipsync/Cargo.toml
git commit -m "feat(clipsync): proto 数据握手 DataReq + HMAC-SHA256 认证"
```

---

## Task 5: file/manifest.rs — 递归遍历建清单（TDD）

**Files:**
- Create: `contrib/dde-kwin-uos/clipsync/src/file/mod.rs`
- Create: `contrib/dde-kwin-uos/clipsync/src/file/manifest.rs`
- Test: 同文件

- [ ] **Step 5.1：file/mod.rs 骨架**

```rust
pub mod manifest;
#[cfg(unix)] pub mod detect_x11;
#[cfg(unix)] pub mod data_server;
#[cfg(windows)] pub mod clipboard_owner;
#[cfg(windows)] pub mod data_object;
#[cfg(windows)] pub mod net_stream;
```

- [ ] **Step 5.2：写失败测试（用 tempfile）**

`Cargo.toml` 加 `[dev-dependencies] tempfile = "3"`。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn walk_dir_produces_relpaths_and_dir_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("myfolder");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("sub/b.bin"), b"xy").unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let built = build_manifest(99, &[root.clone()]);
        let by_rel: std::collections::HashMap<_,_> =
            built.manifest.entries.iter().map(|e| (e.relpath.clone(), e.clone())).collect();

        assert!(by_rel["myfolder"].is_dir);
        assert!(by_rel["myfolder/empty"].is_dir);         // 空目录也有条目
        assert_eq!(by_rel["myfolder/a.txt"].size, 5);
        assert_eq!(by_rel["myfolder/sub/b.bin"].size, 2);
        // file_id → 绝对路径 映射覆盖所有非目录条目
        for e in built.manifest.entries.iter().filter(|e| !e.is_dir) {
            assert!(built.paths.get(&e.file_id).is_some());
        }
    }
}
```

- [ ] **Step 5.3：运行确认失败**

Run: `cargo test --lib walk_dir_produces_relpaths_and_dir_entries`
Expected: FAIL（`build_manifest` 未定义）。

- [ ] **Step 5.4：最小实现**

```rust
use crate::proto::{Entry, Manifest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct BuiltOffer {
    pub manifest: Manifest,
    /// file_id → 绝对路径（仅非目录条目）
    pub paths: HashMap<u32, PathBuf>,
}

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;

/// 输入是用户复制的一组顶层路径（文件或目录），产出扁平清单。
pub fn build_manifest(session_id: u32, roots: &[PathBuf]) -> BuiltOffer {
    let mut entries = Vec::new();
    let mut paths = HashMap::new();
    let mut next_id: u32 = 0;
    for root in roots {
        // relpath 以顶层名开头：复制 /x/myfolder → "myfolder/..."; 复制 /x/a.txt → "a.txt"
        let base = root.parent().unwrap_or(Path::new("/"));
        walk(root, base, &mut entries, &mut paths, &mut next_id, 0);
    }
    BuiltOffer { manifest: Manifest { session_id, entries }, paths }
}

fn walk(
    p: &Path, base: &Path,
    entries: &mut Vec<Entry>, paths: &mut HashMap<u32, PathBuf>,
    next_id: &mut u32, depth: usize,
) {
    if entries.len() >= MAX_ENTRIES || depth > MAX_DEPTH { return; }
    let meta = match std::fs::symlink_metadata(p) { Ok(m) => m, Err(_) => return };
    if meta.file_type().is_symlink() { return; } // 不跟随 symlink，防环
    let relpath = match p.strip_prefix(base) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return,
    };
    if meta.is_dir() {
        let id = *next_id; *next_id += 1;
        entries.push(Entry { file_id: id, relpath, size: 0, is_dir: true });
        if let Ok(rd) = std::fs::read_dir(p) {
            let mut kids: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            kids.sort();
            for k in kids { walk(&k, base, entries, paths, next_id, depth + 1); }
        }
    } else if meta.is_file() {
        let id = *next_id; *next_id += 1;
        entries.push(Entry { file_id: id, relpath, size: meta.len(), is_dir: false });
        paths.insert(id, p.to_path_buf());
    }
}
```

- [ ] **Step 5.5：运行确认通过**

Run: `cargo test --lib walk_dir_produces_relpaths_and_dir_entries`
Expected: PASS。

- [ ] **Step 5.6：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/mod.rs contrib/dde-kwin-uos/clipsync/src/file/manifest.rs contrib/dde-kwin-uos/clipsync/Cargo.toml
git commit -m "feat(clipsync): 递归遍历构建文件 manifest 与 file_id 映射"
```

---

## Task 6: file/detect_x11.rs — uri-list 解析（TDD 纯逻辑部分）

**Files:** Create `contrib/dde-kwin-uos/clipsync/src/file/detect_x11.rs`

- [ ] **Step 6.1：写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn parse_uri_list_decodes_and_filters() {
        let raw = "file:///home/u/a%20b.txt\r\nfile:///home/u/dir\r\n#comment\r\nhttp://x/y\r\n";
        let paths = parse_uri_list(raw);
        assert_eq!(paths, vec![
            PathBuf::from("/home/u/a b.txt"), // percent-decode %20→空格
            PathBuf::from("/home/u/dir"),
        ]); // 注释行与非 file:// 被过滤
    }
}
```

- [ ] **Step 6.2：运行确认失败**

Run: `cargo test --lib parse_uri_list_decodes_and_filters`
Expected: FAIL。

- [ ] **Step 6.3：最小实现**

```rust
use std::path::PathBuf;

/// 解析 text/uri-list：每行一个 URI，`#` 开头为注释；只保留 file:// 本地路径，做 percent-decode。
pub fn parse_uri_list(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        // file:///path → host 为空，剩下 /path；忽略带 host 的远程 URI
        .filter_map(|rest| {
            let path = if let Some(slash) = rest.find('/') { &rest[slash..] } else { return None };
            Some(PathBuf::from(percent_decode(path)))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i+1]), hex(b[i+2])) {
                out.push(h * 16 + l); i += 3; continue;
            }
        }
        out.push(b[i]); i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
fn hex(c: u8) -> Option<u8> {
    match c { b'0'..=b'9' => Some(c - b'0'), b'a'..=b'f' => Some(c - b'a' + 10), b'A'..=b'F' => Some(c - b'A' + 10), _ => None }
}
```

- [ ] **Step 6.4：运行确认通过**

Run: `cargo test --lib parse_uri_list_decodes_and_filters`
Expected: PASS。

- [ ] **Step 6.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/detect_x11.rs
git commit -m "feat(clipsync): X11 uri-list 解析(percent-decode + file:// 过滤)"
```

---

## Task 7: file/detect_x11.rs — xclip 轮询检测（实现 + 手动验证）

**Files:** Modify `detect_x11.rs`

> xclip 调用无法纯单测；用薄封装 + 手动验证。逻辑（去重/建清单）复用已测函数。

- [ ] **Step 7.1：实现 xclip 读取封装**

```rust
use std::process::Command;

fn xclip(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("xclip").args(args).output().ok()?;
    if !out.status.success() { return None; }
    Some(out.stdout)
}

/// 若当前 CLIPBOARD 提供 text/uri-list，返回解析出的本地路径；否则 None。
pub fn read_clipboard_files() -> Option<Vec<std::path::PathBuf>> {
    let targets = xclip(&["-selection", "clipboard", "-t", "TARGETS", "-o"])?;
    let targets = String::from_utf8_lossy(&targets);
    if !targets.lines().any(|l| l.trim() == "text/uri-list") { return None; }
    let uri = xclip(&["-selection", "clipboard", "-t", "text/uri-list", "-o"])?;
    let paths = parse_uri_list(&String::from_utf8_lossy(&uri));
    if paths.is_empty() { None } else { Some(paths) }
}

/// uri-list 内容 hash（去重，避免同一复制重复 offer）。FNV-1a。
pub fn files_hash(paths: &[std::path::PathBuf]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for p in paths { for &b in p.as_os_str().as_encoded_bytes() { h ^= b as u64; h = h.wrapping_mul(0x100000001b3); } h ^= 0xff; h = h.wrapping_mul(0x100000001b3); }
    h
}
```

- [ ] **Step 7.2：编译**

Run: `cargo build`
Expected: PASS（unix 目标）。

- [ ] **Step 7.3：手动验证（在一台 X11/Xwayland Linux 上）**

写一个临时 `examples/probe_files.rs` 或在 main 加临时打印，运行后在文件管理器复制一个文件，观察日志打印出正确本地路径。验证完删除临时代码。

Run: `cargo run --example probe_files`（或临时 main 分支）
Expected: 复制文件后打印出该文件绝对路径；复制文本时返回 None。

⚠️ 待验证点：dde-kwin 环境下 xclip 能否读到文件管理器复制的 uri-list（纯 Wayland 应用可能读不到）。若读不到，记录现象，后续加 wl-paste `-t text/uri-list` 路径。

- [ ] **Step 7.4：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/detect_x11.rs
git commit -m "feat(clipsync): xclip 轮询检测 text/uri-list + 去重 hash"
```

---

## Task 8: file/data_server.rs — 数据服务（tokio，集成测试）

**Files:** Create `contrib/dde-kwin-uos/clipsync/src/file/data_server.rs`

- [ ] **Step 8.1：写失败集成测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::DataReq;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;

    async fn spawn_server(paths: HashMap<u32, std::path::PathBuf>, session: u32, key: Vec<u8>) -> u16 {
        let state = Arc::new(Mutex::new(ServerState { session_id: session, paths }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve(listener, state, key));
        port
    }

    #[tokio::test]
    async fn serves_bytes_from_offset_and_rejects_bad_key() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.bin");
        std::fs::write(&f, b"0123456789").unwrap();
        let mut paths = HashMap::new(); paths.insert(1u32, f);
        let key = b"k".to_vec();
        let port = spawn_server(paths, 7, key.clone()).await;

        // 正确请求：从 offset=3 读
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&DataReq{session_id:7,file_id:1,offset:3}.encode(&key)).await.unwrap();
        let status = s.read_u8().await.unwrap();
        assert_eq!(status, 0);
        let mut buf = Vec::new(); s.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"3456789");

        // 错误 key：连接被拒（读不到 status=0）
        let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s2.write_all(&DataReq{session_id:7,file_id:1,offset:0}.encode(b"wrong")).await.unwrap();
        assert!(s2.read_u8().await.is_err() || s2.read_u8().await.unwrap() != 0);

        // stale session：拒
        let mut s3 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s3.write_all(&DataReq{session_id:999,file_id:1,offset:0}.encode(&key)).await.unwrap();
        assert_ne!(s3.read_u8().await.unwrap(), 0);
    }
}
```

- [ ] **Step 8.2：运行确认失败**

Run: `cargo test --lib serves_bytes_from_offset`
Expected: FAIL。

- [ ] **Step 8.3：最小实现**

```rust
use crate::proto::DataReq;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

pub struct ServerState {
    pub session_id: u32,
    pub paths: HashMap<u32, PathBuf>,
}

pub async fn serve(listener: TcpListener, state: Arc<Mutex<ServerState>>, key: Vec<u8>) {
    loop {
        let (sock, _) = match listener.accept().await { Ok(x) => x, Err(_) => continue };
        let state = state.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, state, key).await { log::debug!("data conn ended: {e}"); }
        });
    }
}

async fn handle(mut sock: TcpStream, state: Arc<Mutex<ServerState>>, key: Vec<u8>) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    let mut hs = [0u8; 53];
    sock.read_exact(&mut hs).await?;
    let req = match DataReq::decode_and_verify(&hs, &key) {
        Ok(r) => r,
        Err(e) => { log::warn!("handshake reject: {e}"); return Ok(()); } // 认证失败直接断，不回 status
    };
    // 查 session + file_id
    let path = {
        let st = state.lock().await;
        if st.session_id != req.session_id { drop(st); let _ = sock.write_u8(1).await; return Ok(()); }
        match st.paths.get(&req.file_id).cloned() {
            Some(p) => p,
            None => { drop(st); let _ = sock.write_u8(2).await; return Ok(()); }
        }
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => { let _ = sock.write_u8(3).await; return Ok(()); }
    };
    use tokio::io::AsyncSeekExt;
    if req.offset > 0 { file.seek(std::io::SeekFrom::Start(req.offset)).await?; }
    sock.write_u8(0).await?; // status OK
    // 固定缓冲 pump，内存恒定
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        sock.write_all(&buf[..n]).await?;
    }
    sock.flush().await?;
    Ok(())
}
```

`Cargo.toml` 的 tokio features 确认含 `fs`、`io-util`、`net`、`rt-multi-thread`。

- [ ] **Step 8.4：运行确认通过**

Run: `cargo test --lib serves_bytes_from_offset`
Expected: PASS（三个断言：offset 读取正确、坏 key 拒、stale 拒）。

- [ ] **Step 8.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/data_server.rs contrib/dde-kwin-uos/clipsync/Cargo.toml
git commit -m "feat(clipsync): Linux data_server 按 offset 流式服务 + HMAC/session 校验"
```

---

## Task 9: Linux 控制侧接线 — 检测→建清单→发 FILE_OFFER + session 管理

**Files:** Modify `main.rs`

- [ ] **Step 9.1：泛化 write_frame 支持任意 type/payload**

在 `main.rs`（或移到 proto）新增：
```rust
async fn write_raw_frame(stream: &mut tokio::net::TcpStream, type_byte: u8, payload: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(type_byte);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf).await?; stream.flush().await
}
```
现有文本 `write_frame` 改为调用它。`read_frame` 改为先读 `type`，按 `FrameKind` 分发：Text→现有路径；FileOffer/FileRevoke→Windows 侧处理（见 Task 14）。

- [ ] **Step 9.2：Linux 侧新增文件轮询任务（#[cfg(unix)]）**

在 `main` 里，仿现有文本轮询，另起一个任务：
```rust
#[cfg(unix)]
{
    let file_tx = file_offer_tx.clone(); // watch/mpsc，送 (Manifest) 给 handle_connection
    let server_state = server_state.clone(); // Arc<Mutex<ServerState>>，与 data_server 共享
    let mut session_counter: u32 = 0;
    let mut last_files_hash: Option<u64> = None;
    tokio::spawn(async move {
        loop {
            if let Some(paths) = file::detect_x11::read_clipboard_files() {
                let h = file::detect_x11::files_hash(&paths);
                if Some(h) != last_files_hash {
                    last_files_hash = Some(h);
                    session_counter += 1;
                    let built = file::manifest::build_manifest(session_counter, &paths);
                    { let mut st = server_state.lock().await; st.session_id = session_counter; st.paths = built.paths; }
                    let _ = file_tx.send(Some(built.manifest));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
}
```
`handle_connection` 的 select! 增加分支：文件 offer channel 变化 → `write_raw_frame(0x10, manifest.to_json_bytes())`。

- [ ] **Step 9.3：启动 data_server**

`main` 里（#[cfg(unix)]）bind `--file-data-port`（新 `Args` 字段，默认如 `4645`），`tokio::spawn(file::data_server::serve(listener, server_state.clone(), key_bytes))`。`key_bytes` 来自 `authentication_key`（新增 `--auth-key` 或复用现有配置来源；与 lan-mouse 一致）。

- [ ] **Step 9.4：编译 + 手动联调（Linux 端）**

Run: `cargo build`
Expected: PASS。手动：Linux 复制文件 → 日志显示发出 FILE_OFFER（此时 Windows 侧还没实现接收，仅验证 Linux 不崩、清单正确）。

- [ ] **Step 9.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/main.rs
git commit -m "feat(clipsync): Linux 侧检测→manifest→FILE_OFFER 接线 + data_server 启动"
```

---

## Task 10: Windows COM 前置 — 注册剪贴板格式 + 构建 FILEGROUPDESCRIPTORW（TDD 可测部分）

**Files:** Create `contrib/dde-kwin-uos/clipsync/src/file/data_object.rs`

> **实现前置**：打开锁定版本 `windows` crate 的 docs.rs，核对以下符号的路径与签名：`RegisterClipboardFormatW`、`FILEGROUPDESCRIPTORW`、`FILEDESCRIPTORW`、`FD_*` 常量、`FILE_ATTRIBUTE_DIRECTORY`、`CFSTR_FILEDESCRIPTORW`/`CFSTR_FILECONTENTS`/`CFSTR_PREFERREDDROPEFFECT`（这三个是字符串常量，用 `RegisterClipboardFormatW(w!("FileGroupDescriptorW"))` 等注册）。

- [ ] **Step 10.1：写失败测试（纯字节布局，不依赖真实剪贴板）**

`FILEGROUPDESCRIPTORW` 内存布局固定（`UINT cItems` + `FILEDESCRIPTORW[cItems]`，`FILEDESCRIPTORW` 592 字节含 `cFileName[260]` WCHAR）。测其构建：
```rust
#[cfg(all(windows, test))]
mod tests {
    use super::*;
    use crate::proto::{Entry, Manifest};
    #[test]
    fn descriptor_has_correct_count_and_names() {
        let m = Manifest { session_id: 1, entries: vec![
            Entry{file_id:0, relpath:"d".into(), size:0, is_dir:true},
            Entry{file_id:1, relpath:"d/a.txt".into(), size:5, is_dir:false},
        ]};
        let blob = build_file_group_descriptor(&m); // Vec<u8>
        let cnt = u32::from_ne_bytes(blob[0..4].try_into().unwrap());
        assert_eq!(cnt, 2);
        // 第 2 条目名应为 "d\a.txt"（反斜杠），UTF-16
        let name2 = read_cfilename(&blob, 1); // helper 读第 i 条 cFileName
        assert_eq!(name2, r"d\a.txt");
        assert!(entry_is_dir(&blob, 0));
        assert_eq!(entry_size(&blob, 1), 5);
    }
}
```

- [ ] **Step 10.2：运行确认失败**

Run: `cargo test --lib descriptor_has_correct_count --target x86_64-pc-windows-msvc`
Expected: FAIL。

- [ ] **Step 10.3：实现（用 repr(C) 镜像结构手工打包，避免依赖 crate 结构体对齐差异）**

```rust
// FILEDESCRIPTORW 布局（Win32 定义，592 bytes）
#[repr(C)]
struct FileDescriptorW {
    dw_flags: u32,
    clsid: [u8; 16],
    sizel: [i32; 2],
    pointl: [i32; 2],
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    file_size_high: u32,
    file_size_low: u32,
    file_name: [u16; 260],
}

const FD_FILESIZE: u32 = 0x40;
const FD_ATTRIBUTES: u32 = 0x04;
const FD_UNICODE: u32 = 0x80000000;
const FD_PROGRESSUI: u32 = 0x4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

pub fn build_file_group_descriptor(m: &crate::proto::Manifest) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(m.entries.len() as u32).to_ne_bytes());
    for e in &m.entries {
        let mut fd: FileDescriptorW = unsafe { std::mem::zeroed() };
        fd.dw_flags = FD_UNICODE | FD_FILESIZE | FD_ATTRIBUTES | FD_PROGRESSUI;
        if e.is_dir {
            fd.file_attributes = FILE_ATTRIBUTE_DIRECTORY;
        } else {
            fd.file_attributes = FILE_ATTRIBUTE_NORMAL;
            fd.file_size_high = (e.size >> 32) as u32;
            fd.file_size_low = (e.size & 0xffff_ffff) as u32;
        }
        let name: Vec<u16> = e.relpath.replace('/', "\\").encode_utf16().take(259).collect();
        fd.file_name[..name.len()].copy_from_slice(&name);
        let bytes = unsafe {
            std::slice::from_raw_parts(&fd as *const _ as *const u8, std::mem::size_of::<FileDescriptorW>())
        };
        out.extend_from_slice(bytes);
    }
    out
}
// + test helpers read_cfilename/entry_is_dir/entry_size 按 4 + i*592 偏移解析
```

- [ ] **Step 10.4：运行确认通过**

Run: `cargo test --lib descriptor_has_correct_count --target x86_64-pc-windows-msvc`
Expected: PASS。

- [ ] **Step 10.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/data_object.rs
git commit -m "feat(clipsync): 构建 FILEGROUPDESCRIPTORW 字节(目录/大小/反斜杠名)"
```

---

## Task 11: file/net_stream.rs — NetStream(IStream over 阻塞 TCP)

**Files:** Create `contrib/dde-kwin-uos/clipsync/src/file/net_stream.rs`

> **实现前置**：核对锁定版本 `windows` crate 中 `IStream_Impl` / `ISequentialStream_Impl` 的 trait 方法签名（`Read(pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT` 等）、`STATSTG`、`STREAM_SEEK`、错误码 `STG_E_READFAULT`。用 `#[implement(IStream)]`。

- [ ] **Step 11.1：先写"纯网络"内核（可测，不含 COM）**

把网络拉取逻辑抽成不依赖 COM 的结构，先 TDD 它：
```rust
use std::io::{Read, Write};
use std::net::TcpStream;
use crate::proto::DataReq;

pub struct FilePuller {
    target: String,       // "host:data_port"
    key: Vec<u8>,
    session_id: u32,
    file_id: u32,
    offset: u64,
    sock: Option<TcpStream>,
}

impl FilePuller {
    pub fn new(target: String, key: Vec<u8>, session_id: u32, file_id: u32) -> Self {
        Self { target, key, session_id, file_id, offset: 0, sock: None }
    }
    fn ensure(&mut self) -> std::io::Result<()> {
        if self.sock.is_some() { return Ok(()); }
        let mut s = TcpStream::connect(&self.target)?;
        s.set_nodelay(true).ok();
        let req = DataReq { session_id: self.session_id, file_id: self.file_id, offset: self.offset };
        s.write_all(&req.encode(&self.key))?;
        let mut status = [0u8; 1];
        s.read_exact(&mut status)?;
        if status[0] != 0 { return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("server status {}", status[0]))); }
        self.sock = Some(s);
        Ok(())
    }
    /// 读 ≤buf.len() 字节，返回实际读到；EOF 返回 0。
    pub fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.ensure()?;
        let n = self.sock.as_mut().unwrap().read(buf)?;
        self.offset += n as u64;
        Ok(n)
    }
    /// 真 seek：关旧连接、置新 offset，下次 read 重开。
    pub fn seek_to(&mut self, new_offset: u64) { self.sock = None; self.offset = new_offset; }
}
```
测试：起 Task 8 的 data_server（localhost + temp 文件），用 `FilePuller` 读全量并与源比对；`seek_to` 后从新 offset 读。

- [ ] **Step 11.2：运行 FilePuller 测试确认通过**

Run: `cargo test --lib file_puller --target x86_64-pc-windows-msvc`
Expected: PASS（把 data_server 测试改成两平台可用，或在 windows 上跑一个最小 tokio server）。

- [ ] **Step 11.3：包一层 COM IStream（对照 docs 落签名）**

```rust
use windows::core::*;
use windows::Win32::System::Com::*;
// #[implement(IStream)] struct NetStream { inner: std::sync::Mutex<FilePuller>, size: u64, pos: u64 }
// impl ISequentialStream_Impl for NetStream_Impl { fn Read(...)->HRESULT { 调 read_chunk，填 pcbRead，EOF 返 S_FALSE，错误返 STG_E_READFAULT } fn Write(...) { STG_E_ACCESSDENIED } }
// impl IStream_Impl for NetStream_Impl {
//   Seek: 计算目标 offset，调 seek_to，回填 plibNewPosition
//   Stat: 填 STATSTG { cbSize: self.size, type: STGTY_STREAM, .. }
//   其余 Clone/SetSize/CopyTo/Commit/Revert/LockRegion/UnlockRegion → 返回 E_NOTIMPL 或 S_OK(无操作)
// }
```
**逐个方法对着 docs.rs 该版本核对参数类型后填充**。取消：`NetStream` 被 Explorer 释放时 `Drop` 关闭 `FilePuller.sock`。

- [ ] **Step 11.4：编译**

Run: `cargo build --target x86_64-pc-windows-msvc`
Expected: PASS。

- [ ] **Step 11.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/net_stream.rs
git commit -m "feat(clipsync): NetStream — 阻塞 TCP 上的 IStream 桥接(含 seek/stat/取消)"
```

---

## Task 12: file/data_object.rs — FileDataObject（IDataObject + AsyncCapability + EnumFORMATETC）

**Files:** Modify `data_object.rs`

> **实现前置**：核对 `IDataObject_Impl`、`IDataObjectAsyncCapability_Impl`、`IEnumFORMATETC_Impl`、`FORMATETC`、`STGMEDIUM`、`TYMED_ISTREAM`/`TYMED_HGLOBAL`、`DVASPECT_CONTENT`、`GlobalAlloc`/`GlobalLock`。

- [ ] **Step 12.1：实现 FileDataObject 骨架**

```rust
// #[implement(IDataObject, IDataObjectAsyncCapability)]
// struct FileDataObject {
//     manifest: crate::proto::Manifest,
//     target: String, key: Vec<u8>,          // 用于给每个文件条目建 NetStream
//     in_async: std::sync::atomic::AtomicBool,
// }
```

- [ ] **Step 12.2：GetData 三格式分发**

- `CFSTR_FILEDESCRIPTORW`：`build_file_group_descriptor(&manifest)` → `GlobalAlloc` HGLOBAL 拷入 → `STGMEDIUM{ tymed: TYMED_HGLOBAL, .. }`。
- `CFSTR_FILECONTENTS`（按 `formatetc.lindex`）：取第 lindex 个**文件**条目的 `file_id`，`NetStream::new(target, key, session_id, file_id, size)` → `STGMEDIUM{ tymed: TYMED_ISTREAM, u.pstm: stream }`。注意 lindex 对应 descriptor 里的**条目下标**（含目录条目）——Explorer 只对文件条目请求 contents，需按下标取 `entries[lindex]` 并断言非目录。
- `CFSTR_PREFERREDDROPEFFECT`：HGLOBAL 装 `DWORD = DROPEFFECT_COPY(=1)`。

- [ ] **Step 12.3：QueryGetData / EnumFormatEtc / IDataObjectAsyncCapability**

- `QueryGetData`：对上述三格式返回 `S_OK`，否则 `DV_E_FORMATETC`。
- `EnumFormatEtc(DATADIR_GET)`：返回列出三格式的 `IEnumFORMATETC`（可用一个简单 `#[implement(IEnumFORMATETC)]` 装 `Vec<FORMATETC>` + 游标）。
- `IDataObjectAsyncCapability`：`SetAsyncMode(TRUE)` 存标志；`GetAsyncMode` 回读；`StartOperation`/`InOperation`/`EndOperation` 维护 `in_async`。
- `GetDataHere`/`SetData`/`DAdvise`/`DUnadvise`/`EnumDAdvise` → `E_NOTIMPL`（`DAdvise` 返回 `OLE_E_ADVISENOTSUPPORTED`）。

- [ ] **Step 12.4：编译**

Run: `cargo build --target x86_64-pc-windows-msvc`
Expected: PASS。

- [ ] **Step 12.5：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/data_object.rs
git commit -m "feat(clipsync): FileDataObject 实现 IDataObject/Async/EnumFORMATETC 三格式"
```

---

## Task 13: file/clipboard_owner.rs — STA 线程 + OleSetClipboard + 消息泵

**Files:** Create `contrib/dde-kwin-uos/clipsync/src/file/clipboard_owner.rs`

> **实现前置**：核对 `OleInitialize`、`OleSetClipboard`、`OleUninitialize`、`CreateWindowExW`(message-only, HWND_MESSAGE)、`GetMessageW`/`TranslateMessage`/`DispatchMessageW`、`PostThreadMessageW`。STA：`OleInitialize` 隐含 `CoInitialize(APARTMENTTHREADED)`。

- [ ] **Step 13.1：实现 owner 线程入口**

```rust
pub enum OwnerMsg { SetOffer(crate::proto::Manifest), Clear }

/// 在专用 OS 线程调用；内部 OleInitialize + 消息泵。收到 SetOffer 就 OleSetClipboard(FileDataObject)。
pub fn run_owner(rx: std::sync::mpsc::Receiver<OwnerMsg>, target: String, key: Vec<u8>) {
    // OleInitialize(None)
    // 建 message-only 窗口（或纯用 PostThreadMessage + 在 GetMessage 循环里 poll rx）
    // 循环：
    //   - 用带超时的方式检查 rx（如 MsgWaitForMultipleObjects，或每次 GetMessage 前 try_recv）
    //   - SetOffer(m): let obj: IDataObject = FileDataObject{manifest:m, target, key,..}.into();
    //                  OleSetClipboard(&obj).ok();
    //   - Clear: OleSetClipboard(None)
    // 退出前 OleUninitialize()
}
```
> 关键：STA 必须泵消息，否则 OLE 剪贴板封送会死锁。用 `MsgWaitForMultipleObjectsEx` 同时等"新 offer 事件"和窗口消息，是最稳的写法——实现时按此核对。

- [ ] **Step 13.2：编译**

Run: `cargo build --target x86_64-pc-windows-msvc`
Expected: PASS。

- [ ] **Step 13.3：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/file/clipboard_owner.rs
git commit -m "feat(clipsync): Windows STA 剪贴板 owner 线程 + OleSetClipboard + 消息泵"
```

---

## Task 14: Windows 控制侧接线 — 收 FILE_OFFER → 交给 owner 线程

**Files:** Modify `main.rs`

- [ ] **Step 14.1：启动 owner 线程 + 建 channel（#[cfg(windows)]）**

```rust
#[cfg(windows)]
let owner_tx = {
    let (tx, rx) = std::sync::mpsc::channel::<file::clipboard_owner::OwnerMsg>();
    let target = format!("{}:{}", connect_host, args.file_data_port); // connect_host 来自 --connect
    let key = key_bytes.clone();
    std::thread::spawn(move || file::clipboard_owner::run_owner(rx, target, key));
    tx
};
```

- [ ] **Step 14.2：read_frame 分发 FILE_OFFER**

在 `handle_connection` 的读帧分支里，`FrameKind::FileOffer` → `Manifest::from_json_bytes(payload)` → `owner_tx.send(OwnerMsg::SetOffer(m))`；`FileRevoke` → `OwnerMsg::Clear`；`Text` → 现有 `write_local_clipboard` 路径。

- [ ] **Step 14.3：编译（两平台）**

Run: `cargo build` 和 `cargo build --target x86_64-pc-windows-msvc`
Expected: PASS。

- [ ] **Step 14.4：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/src/main.rs
git commit -m "feat(clipsync): Windows 侧收 FILE_OFFER 交 owner 线程上剪贴板"
```

---

## Task 15: 端到端"假粘贴器"测试（不靠 Explorer）

**Files:** Create `contrib/dde-kwin-uos/clipsync/examples/fake_paster.rs`

- [ ] **Step 15.1：实现假粘贴器**

`OleInitialize` → `OleGetClipboard()` 拿 `IDataObject` → `GetData(CFSTR_FILEDESCRIPTORW)` 解析条目 → 对每个文件条目 `GetData(CFSTR_FILECONTENTS, lindex=i, TYMED_ISTREAM)` 拿 `IStream` → 循环 `Read` 到 EOF 写入本地临时目录 → 与源目录逐字节比对；打印结果。

- [ ] **Step 15.2：本机自环验证**

在一台机器上同时跑：一个 clipsync（Linux 模式模拟不便，故改为）——**改用最小自环**：临时在 Windows 上起一个 `data_server` 等价的本地 server + 直接构造 `FileDataObject` 调 `OleSetClipboard`，再跑 `fake_paster`。断言字节一致。

Run: `cargo run --example fake_paster --target x86_64-pc-windows-msvc`
Expected: 打印"MATCH"，所有文件字节一致。

- [ ] **Step 15.3：Commit**

```bash
git add contrib/dde-kwin-uos/clipsync/examples/fake_paster.rs
git commit -m "test(clipsync): 假粘贴器端到端覆盖 IDataObject+IStream+网络路径"
```

---

## Task 16: 真机联调 + connect.ps1 接线 + 手动验收

**Files:** Modify `contrib/dde-kwin-uos/connect.ps1`（数据端口 + 启动参数）

- [ ] **Step 16.1：connect.ps1 加数据端口参数与防火墙**

给 Windows 端 clipsync 启动命令加 `--file-data-port <port>`（与 Linux 端一致），Linux 端同样。确认防火墙放行该 TCP 端口（Linux 侧监听）。

- [ ] **Step 16.2：真机手动验收清单**

在真实 Linux(X11/Xwayland) + Windows 部署，逐项验证：
- [ ] 单个小文件：远端复制 → 本地桌面 Ctrl+V → 文件出现、内容一致、有进度框。
- [ ] 多文件框选一次复制 → 全部落地。
- [ ] 文件夹（含子目录、空目录）→ 目录树完整重建。
- [ ] GB 级大文件 → 进度框正常推进、内存不爆、可取消（取消后无残留半文件由 Explorer 清理）。
- [ ] 中文/空格文件名 → 正确。
- [ ] 复制后在远端再复制别的，再回本地粘贴旧内容 → 干净失败（stale），不卡死。
- [ ] 传输中断网 → Explorer 报错而非无限卡。

- [ ] **Step 16.3：Commit**

```bash
git add contrib/dde-kwin-uos/connect.ps1
git commit -m "feat(connect.ps1): 接入 clipsync 文件数据端口与启动参数"
```

---

## 自查（写完计划对照 spec）

- **Spec 覆盖**：范围(§1)→全 Task；协议(§5)→Task 2/3/4；Linux(§6)→Task 5/6/7/8/9；Windows(§7)→Task 10/11/12/13/14；错误处理(§8)→分散在 Task 8(stale/坏key/缺文件)、11(取消/网络错)、16(验收)；测试(§9)→Task 2–8 单测/集成 + Task 15 假粘贴器 + Task 16 人工。
- **占位扫描**：COM 任务中标注"对照 docs.rs 核对签名"是**真实 FFI 落地步骤**（锁定版本后即为确定动作），非"add error handling"式空占位；纯逻辑/Linux 任务均给完整代码。
- **类型一致**：`Manifest`/`Entry`/`DataReq`/`BuiltOffer`/`ServerState`/`FilePuller`/`OwnerMsg` 跨任务命名一致；`file_id:u32`、`session_id:u32`、`offset:u64` 全程统一。
- **已知取舍**：COM 三个任务(11/12/13)的 vtable 细节依赖锁定 crate 版本，故给骨架 + 逐方法核对步骤而非逐字节代码——这是对 FFI 不确定性的诚实处理，避免编造错误 API。
