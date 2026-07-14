//! Linux 发送侧：X11 剪贴板文件检测（`#[cfg(unix)]`）。
//!
//! 用 `xclip` 轮询 CLIPBOARD 的 `TARGETS`；若提供 `text/uri-list` 就读取并
//! 解析出本地 `file://` 路径。解析（percent-decode + 过滤）是纯逻辑、可单测；
//! xclip 调用是薄封装，靠手动/真机验证。`files_hash` 供轮询去重，避免同一次
//! 复制重复发 FILE_OFFER。
//!
//! ⚠️ 待真机验证：dde-kwin(Wayland) 下文件管理器复制的 uri-list 能否被 xclip
//! （X11/Xwayland）读到；若读不到需后续补 `wl-paste -t text/uri-list` 路径。

use std::path::PathBuf;
use std::process::Command;

// ===========================================================================
// 纯逻辑：uri-list 解析（可单测）
// ===========================================================================

/// 解析 `text/uri-list`：每行一个 URI，`#` 开头为注释；只保留 `file://` 本地
/// 路径并做 percent-decode。带 host 的远程 URI / 非 file 协议被过滤。
pub fn parse_uri_list(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        // file:///path → host 为空，剩下 /path；忽略带 host 的远程 URI
        .filter_map(|rest| {
            // rest = <host>/path（file:// 已剥离）
            // file:///path → host 为空字符串，放行。
            // file://localhost/path → host == "localhost"，放行。
            // file://otherhost/path → host 非空且不是 localhost，丢弃。
            let slash_pos = rest.find('/')?;
            let host = &rest[..slash_pos];
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return None;
            }
            Some(PathBuf::from(percent_decode(&rest[slash_pos..])))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ===========================================================================
// xclip 封装：检测 CLIPBOARD 是否含 text/uri-list（薄封装，手动验证）
// ===========================================================================

fn xclip(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("xclip").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// 若当前 CLIPBOARD 提供 `text/uri-list`，返回解析出的本地路径；否则 `None`。
pub fn read_clipboard_files() -> Option<Vec<PathBuf>> {
    let targets = xclip(&["-selection", "clipboard", "-t", "TARGETS", "-o"])?;
    let targets = String::from_utf8_lossy(&targets);
    if !targets.lines().any(|l| l.trim() == "text/uri-list") {
        return None;
    }
    let uri = xclip(&["-selection", "clipboard", "-t", "text/uri-list", "-o"])?;
    let paths = parse_uri_list(&String::from_utf8_lossy(&uri));
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// uri-list 内容 hash（去重，避免同一复制重复 offer）。FNV-1a。
pub fn files_hash(paths: &[PathBuf]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for p in paths {
        for &b in p.as_os_str().as_encoded_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_uri_list_decodes_and_filters() {
        let raw = "file:///home/u/a%20b.txt\r\nfile:///home/u/dir\r\n#comment\r\nhttp://x/y\r\n";
        let paths = parse_uri_list(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/u/a b.txt"), // percent-decode %20→空格
                PathBuf::from("/home/u/dir"),
            ]
        ); // 注释行与非 file:// 被过滤
    }

    #[test]
    fn parse_uri_list_rejects_remote_host_accepts_localhost() {
        // file://otherhost/x 被拒；file:///x 和 file://localhost/x 放行。
        let raw = "file://otherhost/x\r\nfile:///home/u/b.txt\r\nfile://localhost/tmp/c.txt\r\n";
        let paths = parse_uri_list(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/u/b.txt"),
                PathBuf::from("/tmp/c.txt"),
            ]
        );
    }
}
