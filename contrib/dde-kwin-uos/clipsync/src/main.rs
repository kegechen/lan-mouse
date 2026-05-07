//! clipsync — 跨平台双向剪贴板同步 daemon。
//!
//! 设计：单 TCP 连接 + TLV 帧协议 + 防 echo（hash 去重）+ 500ms 轮询。
//!
//! ## 帧协议（version 1）
//!
//! 每帧独立：
//! ```text
//! [u8 type][u32 BE length][N bytes payload]
//! ```
//!
//! type:
//!   - `0x01` = UTF-8 text
//!   - `0x02` = PNG image    (RESERVED, not yet implemented)
//!   - `0x03` = files list   (RESERVED, not yet implemented)
//!
//! ## 扩展点
//!
//! 添加新 clipboard 类型时改 4 个地方：
//! 1. `ClipboardItem` enum 加 variant
//! 2. `ClipboardItem::type_byte()` / `payload()` 加分支
//! 3. `parse_frame()` 加 type 分支
//! 4. `read_local_clipboard()` / `write_local_clipboard()` 加平台实现

use clap::Parser;
use log::{debug, error, info, warn};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(about = "Tiny bidirectional clipboard sync (text now, image planned)")]
struct Args {
    /// 监听模式：listen 在指定地址等对端连接（被动端，如 UOS）
    #[arg(long, value_name = "IP:PORT", group = "mode")]
    listen: Option<String>,

    /// 连接模式：connect 到对端地址（主动端，如 Windows）
    #[arg(long, value_name = "HOST:PORT", group = "mode")]
    connect: Option<String>,

    /// 轮询本地剪贴板的间隔（毫秒）
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,

    /// 重连间隔（仅 connect 模式生效）
    #[arg(long, default_value_t = 3000)]
    reconnect_ms: u64,
}

// ===========================================================================
// ClipboardItem — 扩展点：添加新类型在此 enum 加 variant
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClipboardItem {
    Text(String),
    // Image(Vec<u8>),         // PNG bytes — TODO
    // Files(Vec<String>),     // file paths — TODO
}

impl ClipboardItem {
    fn type_byte(&self) -> u8 {
        match self {
            ClipboardItem::Text(_) => 0x01,
            // ClipboardItem::Image(_) => 0x02,
            // ClipboardItem::Files(_) => 0x03,
        }
    }

    fn payload(&self) -> Vec<u8> {
        match self {
            ClipboardItem::Text(s) => s.as_bytes().to_vec(),
            // ClipboardItem::Image(b) => b.clone(),
            // ClipboardItem::Files(list) => list.join("\n").into_bytes(),
        }
    }

    fn from_frame(type_byte: u8, payload: Vec<u8>) -> Result<Self, String> {
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
    fn hash(&self) -> u64 {
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
fn read_local_clipboard() -> Option<ClipboardItem> {
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
fn write_local_clipboard(item: &ClipboardItem) -> Result<(), String> {
    let mut clip = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    match item {
        ClipboardItem::Text(s) => clip.set_text(s).map_err(|e| e.to_string()),
        // ClipboardItem::Image(b) => ...
    }
}

#[cfg(unix)]
fn unix_try_read(cmd: &str, args: &[&str]) -> Option<String> {
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
fn read_local_clipboard() -> Option<ClipboardItem> {
    // 优先 wl-paste：dde-kwin Wayland session 下 GUI 应用复制的内容在 Wayland clipboard
    // 桥接到 Xwayland 单/双向不可靠，wl-paste 直接读 wayland 来源最准。
    // 失败 fallback xclip（X11 应用复制 / 我们自己 wl-copy 失败时的兜底）。
    let text = unix_try_read("wl-paste", &["--no-newline"])
        .or_else(|| unix_try_read("xclip", &["-selection", "clipboard", "-o"]))?;
    Some(ClipboardItem::Text(text))
}

#[cfg(unix)]
fn unix_write_via(cmd: &str, args: &[&str], data: &[u8]) -> Result<(), String> {
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
fn write_local_clipboard(item: &ClipboardItem) -> Result<(), String> {
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

// ===========================================================================
// Frame I/O
// ===========================================================================

async fn write_frame(stream: &mut TcpStream, item: &ClipboardItem) -> std::io::Result<()> {
    let payload = item.payload();
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(item.type_byte());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> std::io::Result<ClipboardItem> {
    let type_byte = stream.read_u8().await?;
    let len = stream.read_u32().await? as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len}"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    ClipboardItem::from_frame(type_byte, buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })
}

// ===========================================================================
// 主循环
// ===========================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // 防 echo：刚刚同步过的内容 hash（双向都用此 hash 拒收/拒发）
    let last_hash: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    // 本地剪贴板变化通知 channel（watch = 始终保留最新值，丢弃旧的）
    let (clip_tx, clip_rx) = watch::channel::<Option<ClipboardItem>>(None);

    // 启动本地 clipboard 轮询任务
    {
        let last_hash = last_hash.clone();
        let poll_ms = args.poll_ms;
        tokio::spawn(async move {
            let mut last_seen: Option<u64> = None;
            loop {
                if let Some(item) = read_local_clipboard() {
                    let h = item.hash();
                    let synced_h = *last_hash.lock().unwrap();
                    // 跳过自己刚 write 进去的（peer 推过来的，会触发 read 检测到）
                    if Some(h) == synced_h {
                        last_seen = Some(h);
                    } else if Some(h) != last_seen {
                        debug!("local clipboard changed (hash {h:x})");
                        last_seen = Some(h);
                        let _ = clip_tx.send(Some(item));
                    }
                }
                sleep(Duration::from_millis(poll_ms)).await;
            }
        });
    }

    // 网络主循环：listen 或 connect，断开自动重连
    loop {
        let stream_result = if let Some(addr) = &args.listen {
            accept_one(addr).await
        } else if let Some(addr) = &args.connect {
            connect_one(addr).await
        } else {
            error!("must specify --listen or --connect");
            return Ok(());
        };

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                warn!("connection error: {e}; retry in {}ms", args.reconnect_ms);
                sleep(Duration::from_millis(args.reconnect_ms)).await;
                continue;
            }
        };

        info!("connected: {:?}", stream.peer_addr().ok());
        if let Err(e) = handle_connection(stream, clip_rx.clone(), last_hash.clone()).await {
            warn!("session ended: {e}");
        }
        sleep(Duration::from_millis(args.reconnect_ms)).await;
    }
}

async fn accept_one(addr: &str) -> std::io::Result<TcpStream> {
    let listener = TcpListener::bind(addr).await?;
    info!("listening on {addr}, waiting for peer...");
    let (stream, _) = listener.accept().await?;
    Ok(stream)
}

async fn connect_one(addr: &str) -> std::io::Result<TcpStream> {
    info!("connecting to {addr}...");
    TcpStream::connect(addr).await
}

async fn handle_connection(
    mut stream: TcpStream,
    mut clip_rx: watch::Receiver<Option<ClipboardItem>>,
    last_hash: Arc<Mutex<Option<u64>>>,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    loop {
        tokio::select! {
            // 本地 clipboard 变了 → 写到 peer
            r = clip_rx.changed() => {
                if r.is_err() { break; }
                let item_opt = clip_rx.borrow().clone();
                if let Some(item) = item_opt {
                    let h = item.hash();
                    {
                        let synced = *last_hash.lock().unwrap();
                        if Some(h) == synced {
                            // 这条本身就是 peer 刚送来的，别再回送（防 echo）
                            continue;
                        }
                    }
                    if let Err(e) = write_frame(&mut stream, &item).await {
                        warn!("write_frame failed: {e}");
                        return Err(e);
                    }
                    debug!("sent to peer (hash {h:x}, type 0x{:02x})", item.type_byte());
                }
            }
            // peer 推剪贴板内容 → 写到本地
            r = read_frame(&mut stream) => {
                let item = r?;
                let h = item.hash();
                *last_hash.lock().unwrap() = Some(h);
                if let Err(e) = write_local_clipboard(&item) {
                    warn!("write_local_clipboard failed: {e}");
                } else {
                    debug!("received from peer (hash {h:x}, type 0x{:02x})", item.type_byte());
                }
            }
        }
    }
    Ok(())
}
