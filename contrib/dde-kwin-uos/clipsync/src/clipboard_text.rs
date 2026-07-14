//! 文本剪贴板读写（从 main.rs 抽出，行为完全不变）。

// ===========================================================================
// ClipboardItem — 扩展点：添加新类型在此 enum 加 variant
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardItem {
    Text(String),
    // Image(Vec<u8>),         // PNG bytes — TODO
    // Files(Vec<String>),     // file paths — TODO
}

impl ClipboardItem {
    pub fn type_byte(&self) -> u8 {
        match self {
            ClipboardItem::Text(_) => 0x01,
            // ClipboardItem::Image(_) => 0x02,
            // ClipboardItem::Files(_) => 0x03,
        }
    }

    pub fn payload(&self) -> Vec<u8> {
        match self {
            ClipboardItem::Text(s) => s.as_bytes().to_vec(),
            // ClipboardItem::Image(b) => b.clone(),
            // ClipboardItem::Files(list) => list.join("\n").into_bytes(),
        }
    }

    pub fn from_frame(type_byte: u8, payload: Vec<u8>) -> Result<Self, String> {
        match type_byte {
            0x01 => String::from_utf8(payload)
                .map(ClipboardItem::Text)
                .map_err(|e| format!("invalid utf-8 text: {e}")),
            // 0x02 => Ok(ClipboardItem::Image(payload)),
            // 0x03 => Ok(ClipboardItem::Files(...)),
            other => Err(format!("unknown frame type 0x{other:02x}")),
        }
    }

    /// FNV-1a 64-bit hash（防 echo，比较内容是否相同）
    pub fn hash(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        h ^= self.type_byte() as u64;
        h = h.wrapping_mul(0x100000001b3);
        for &b in self.payload().iter() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
}

// ===========================================================================
// 平台层：read/write local clipboard
// ===========================================================================

#[cfg(windows)]
pub fn read_local_clipboard() -> Option<ClipboardItem> {
    // arboard get_text 在 Win32 上用 OpenClipboard + GetClipboardData
    let mut clip = arboard::Clipboard::new().ok()?;
    let text = clip.get_text().ok()?;
    if text.is_empty() {
        return None;
    }
    Some(ClipboardItem::Text(text))
    // TODO: clip.get_image() → Image(png_bytes)
}

#[cfg(windows)]
pub fn write_local_clipboard(item: &ClipboardItem) -> Result<(), String> {
    let mut clip = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    match item {
        ClipboardItem::Text(s) => clip.set_text(s).map_err(|e| e.to_string()),
        // ClipboardItem::Image(b) => ...
    }
}

#[cfg(unix)]
pub fn unix_try_read(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(unix)]
pub fn read_local_clipboard() -> Option<ClipboardItem> {
    // 优先 wl-paste：dde-kwin Wayland session 下 GUI 应用复制的内容在 Wayland clipboard
    // 桥接到 Xwayland 单/双向不可靠，wl-paste 直接读 wayland 来源最准。
    // 失败 fallback xclip（X11 应用复制 / 我们自己 wl-copy 失败时的兜底）。
    let text = unix_try_read("wl-paste", &["--no-newline"])
        .or_else(|| unix_try_read("xclip", &["-selection", "clipboard", "-o"]))?;
    Some(ClipboardItem::Text(text))
}

#[cfg(unix)]
pub fn unix_write_via(cmd: &str, args: &[&str], data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {cmd}: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("no stdin handle")?
        .write_all(data)
        .map_err(|e| format!("write {cmd}: {e}"))?;
    drop(child.stdin.take());
    let _ = child.wait().map_err(|e| format!("wait {cmd}: {e}"))?;
    Ok(())
}

#[cfg(unix)]
pub fn write_local_clipboard(item: &ClipboardItem) -> Result<(), String> {
    match item {
        ClipboardItem::Text(s) => {
            // 同时写 X11 clipboard (xclip) 和 Wayland clipboard (wl-copy)：
            // - dde-kwin 5.15 单向桥接（Wayland → X11）；X 写入 Wayland 看不见
            // - 同时写两边 → X 应用看 X11 clipboard，Wayland 应用看 Wayland clipboard
            // - wl-copy 老版即使 daemon 不持久，KDE klipper 通常会接管 owner 缓存
            let bytes = s.as_bytes();
            let r1 = unix_write_via("xclip", &["-selection", "clipboard", "-i"], bytes);
            let r2 = unix_write_via("wl-copy", &[], bytes);
            // 任一成功即视为成功
            if r1.is_ok() || r2.is_ok() {
                if r1.is_err() {
                    log::debug!("xclip write failed: {:?}", r1);
                }
                if r2.is_err() {
                    log::debug!("wl-copy write failed: {:?}", r2);
                }
                Ok(())
            } else {
                Err(format!("both xclip and wl-copy failed: {:?} / {:?}", r1, r2))
            }
        }
    }
}
