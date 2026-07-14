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
//!
//! 文本剪贴板逻辑（`ClipboardItem` 及平台 read/write）已抽到
//! `clipboard_text` 模块；文件传输协议/清单在 `proto` / `file`。

use clap::Parser;
use clipsync::clipboard_text::{read_local_clipboard, write_local_clipboard, ClipboardItem};
use log::{debug, error, info, warn};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::sleep;

#[cfg(windows)]
use clipsync::file::clipboard_owner::{run_owner, OwnerMsg};

/// 文件帧转发句柄：Windows 上是发往 owner 线程的 `Sender`；其它平台为占位 `()`。
///
/// `handle_connection` 收到 FILE_OFFER/FILE_REVOKE 帧时经此转给 STA owner 线程；
/// 非 Windows 侧（Linux 是发送方）不接收此类帧，故用零成本占位保持签名一致。
#[cfg(windows)]
type FileSink = std::sync::mpsc::Sender<OwnerMsg>;
#[cfg(not(windows))]
type FileSink = ();

/// 文件 offer 来源：Linux(unix) 发送侧是本机文件剪贴板轮询产出的 `Manifest`
/// 通道（`watch`，始终保留最新一次复制的清单）；其它平台为占位 `()`。
///
/// `handle_connection` 在 `select!` 里监听它，一旦本机复制了新文件就写
/// FILE_OFFER(0x10) 帧给 peer。Windows 是接收侧、不产生 offer，故用零成本占位。
#[cfg(unix)]
type FileOfferSource = watch::Receiver<Option<clipsync::proto::Manifest>>;
#[cfg(not(unix))]
type FileOfferSource = ();

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

    /// 文件数据连接端口：Windows 粘贴时连对端此端口按需拉文件字节（与 Linux 端一致）。
    #[arg(long, default_value_t = 4645)]
    file_data_port: u16,

    /// 数据连接 HMAC 共享密钥（与对端 authentication_key 一致）。空串仅用于测试，不安全。
    #[arg(long, default_value = "")]
    data_key: String,
}

// ===========================================================================
// Frame I/O
// ===========================================================================

/// 写一帧任意 `type/payload`（线上格式 `[u8 type][u32 BE len][payload]`）。
/// 文本帧与 FILE_OFFER 帧都走它，保证线上编码一致。
async fn write_raw_frame(
    stream: &mut TcpStream,
    type_byte: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(type_byte);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf).await?;
    stream.flush().await
}

async fn write_frame(stream: &mut TcpStream, item: &ClipboardItem) -> std::io::Result<()> {
    write_raw_frame(stream, item.type_byte(), &item.payload()).await
}

/// 取本机下一次文件 offer（供 `handle_connection` 的 `select!` 使用）。
///
/// unix 发送侧：等 `watch` 通道产生新 `Manifest`；通道被关闭（轮询任务常驻，
/// 正常不会发生）时退化为永挂，避免忙轮询。其它平台：永远挂起（Windows 是
/// 接收侧，不产生 offer），使该 `select!` 分支永不触发。
#[cfg(unix)]
async fn next_file_offer(rx: &mut FileOfferSource) -> Option<clipsync::proto::Manifest> {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
    rx.borrow().clone()
}

#[cfg(not(unix))]
async fn next_file_offer(_rx: &mut FileOfferSource) -> Option<clipsync::proto::Manifest> {
    std::future::pending::<Option<clipsync::proto::Manifest>>().await
}

/// 读一帧的原始 `(type_byte, payload)`，由调用方按类型分发（Text/FileOffer/…）。
async fn read_raw_frame(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
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
    Ok((type_byte, buf))
}

/// 控制连接认证用的一次性 nonce：wall-clock 纳秒(8B) + 进程内递增计数(8B)，
/// 保证每次连接 nonce 新鲜（防重放）。挑战-应答的安全性来自 key，而非 nonce
/// 不可预测性，故无需密码学随机数。
fn make_ctrl_nonce() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let ctr = CTR.fetch_add(1, Ordering::Relaxed);
    let mut n = [0u8; 16];
    n[0..8].copy_from_slice(&nanos.to_be_bytes());
    n[8..16].copy_from_slice(&ctr.to_be_bytes());
    n
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

    // Windows：启动 STA 剪贴板 owner 线程 + 建 mpsc 通道；handle_connection 收到
    // FILE_OFFER/FILE_REVOKE 时经 file_sink 转给它。数据目标 = connect 主机 + 数据端口。
    #[cfg(windows)]
    let file_sink: FileSink = {
        let (tx, rx) = std::sync::mpsc::channel::<OwnerMsg>();
        let host = args
            .connect
            .as_deref()
            .and_then(|c| c.rsplit_once(':').map(|(h, _)| h.to_string()))
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let target = format!("{host}:{}", args.file_data_port);
        let key = args.data_key.clone().into_bytes();
        if key.is_empty() {
            warn!("--data-key empty: file paste will fail against a secured peer (data pulls are HMAC-authenticated)");
        }
        std::thread::spawn(move || run_owner(rx, target, key));
        info!(
            "clipboard file-offer owner thread spawned (data target {}:{})",
            host, args.file_data_port
        );
        tx
    };
    #[cfg(not(windows))]
    let file_sink: FileSink = ();

    // Linux(unix) 发送侧：起独立数据服务（0.0.0.0:file_data_port，HMAC=data_key）
    // + 文件剪贴板轮询。轮询检测到新一组复制文件时递增 session、重建 manifest、
    // 更新与 data_server 共享的 ServerState，并把 Manifest 经 watch 通道送
    // handle_connection 发 FILE_OFFER。
    #[cfg(unix)]
    let file_offer_rx: FileOfferSource = {
        use std::collections::HashMap;

        let (file_offer_tx, file_offer_rx) =
            watch::channel::<Option<clipsync::proto::Manifest>>(None);

        // fail-closed：空 HMAC key 下任何主机都能伪造合法握手拉文件 → 不启用文件功能
        // （文本同步不受影响）。
        let file_enabled = !args.data_key.is_empty();
        if !file_enabled {
            error!("file transfer DISABLED: --data-key is required (empty key would let any LAN host pull files); text sync unaffected");
        }

        // 与 data_server 共享：复制新文件组时递增 session_id 并替换 paths，
        // 旧 session 的数据请求随即被拒（stale）。
        let server_state = Arc::new(tokio::sync::Mutex::new(
            clipsync::file::data_server::ServerState {
                session_id: 0,
                paths: HashMap::new(),
            },
        ));

        // 数据服务：按需回传文件字节（Windows 粘贴时连本机此端口）
        if file_enabled {
            let state = server_state.clone();
            let key = args.data_key.clone().into_bytes();
            let port = args.file_data_port;
            tokio::spawn(async move {
                match TcpListener::bind(("0.0.0.0", port)).await {
                    Ok(listener) => {
                        info!("file data server listening on 0.0.0.0:{port}");
                        clipsync::file::data_server::serve(listener, state, key).await;
                    }
                    Err(e) => error!("file data server bind 0.0.0.0:{port} failed: {e}"),
                }
            });
        }

        // 文件剪贴板轮询（500ms）：检测 uri-list → 去重 → 建 manifest → 更新
        // 共享 state + 经通道送 offer。
        if file_enabled {
            let server_state = server_state.clone();
            tokio::spawn(async move {
                let mut session_counter: u32 = 0;
                let mut last_files_hash: Option<u64> = None;
                loop {
                    if let Some(paths) = clipsync::file::detect_x11::read_clipboard_files() {
                        let h = clipsync::file::detect_x11::files_hash(&paths);
                        if Some(h) != last_files_hash {
                            last_files_hash = Some(h);
                            session_counter += 1;
                            let built =
                                clipsync::file::manifest::build_manifest(session_counter, &paths);
                            {
                                let mut st = server_state.lock().await;
                                st.session_id = session_counter;
                                st.paths = built.paths;
                            }
                            info!(
                                "file clipboard changed: session {}, {} entries",
                                session_counter,
                                built.manifest.entries.len()
                            );
                            let _ = file_offer_tx.send(Some(built.manifest));
                        }
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            });
        }

        file_offer_rx
    };
    #[cfg(not(unix))]
    let file_offer_rx: FileOfferSource = ();

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

        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                warn!("connection error: {e}; retry in {}ms", args.reconnect_ms);
                sleep(Duration::from_millis(args.reconnect_ms)).await;
                continue;
            }
        };

        info!("connected: {:?}", stream.peer_addr().ok());
        // 控制连接认证：仅在配置了共享 key（文件功能启用）时强制；双方对称配置故一致。
        // 认证保护整条控制通道（含 FILE_OFFER），堵住未授权 LAN 主机注入 offer/文本。
        if !args.data_key.is_empty() {
            // 10s 超时：防 stall 的对端（或两端 key 配置不一致）令握手 read_exact 永挂、
            // 卡死主循环（与 data_server 握手超时一致）。超时/失败均丢连接重连。
            let hs = clipsync::proto::control_handshake(
                &mut stream,
                args.data_key.as_bytes(),
                make_ctrl_nonce(),
            );
            match tokio::time::timeout(Duration::from_secs(10), hs).await {
                Ok(Ok(())) => info!("control channel authenticated"),
                Ok(Err(e)) => {
                    warn!("control auth failed: {e}; dropping connection");
                    sleep(Duration::from_millis(args.reconnect_ms)).await;
                    continue;
                }
                Err(_) => {
                    warn!("control auth timed out; dropping connection");
                    sleep(Duration::from_millis(args.reconnect_ms)).await;
                    continue;
                }
            }
        }
        if let Err(e) = handle_connection(
            stream,
            clip_rx.clone(),
            last_hash.clone(),
            file_sink.clone(),
            file_offer_rx.clone(),
        )
        .await
        {
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
    file_sink: FileSink,
    mut file_offer_rx: FileOfferSource,
) -> std::io::Result<()> {
    #[cfg(not(windows))]
    let _ = &file_sink; // 占位句柄在非 Windows 未用
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
            // 本机文件剪贴板产生新 offer（仅 unix 发送侧会触发）→ 写 FILE_OFFER 帧
            offer = next_file_offer(&mut file_offer_rx) => {
                if let Some(manifest) = offer {
                    let bytes = manifest.to_json_bytes();
                    if bytes.len() > clipsync::proto::MAX_OFFER_BYTES {
                        // 超帧上限的 offer 直接跳过 + 明确日志，别发一个 peer 必拒的帧
                        // 导致 read_raw_frame「frame too large」→ 断连 → 重连循环。
                        warn!(
                            "FILE_OFFER too large: {} bytes / {} entries — skipped (peer frame cap 16MB)",
                            bytes.len(),
                            manifest.entries.len()
                        );
                        continue;
                    }
                    if let Err(e) = write_raw_frame(&mut stream, 0x10, &bytes).await {
                        warn!("write FILE_OFFER failed: {e}");
                        return Err(e);
                    }
                    debug!(
                        "sent FILE_OFFER (session {}, {} entries)",
                        manifest.session_id,
                        manifest.entries.len()
                    );
                }
            }
            // peer 推来一帧 → 按类型分发
            r = read_raw_frame(&mut stream) => {
                let (type_byte, payload) = r?;
                match type_byte {
                    // 0x01 文本：写本地剪贴板（原有路径，行为不变）
                    0x01 => match ClipboardItem::from_frame(type_byte, payload) {
                        Ok(item) => {
                            let h = item.hash();
                            *last_hash.lock().unwrap() = Some(h);
                            if let Err(e) = write_local_clipboard(&item) {
                                warn!("write_local_clipboard failed: {e}");
                            } else {
                                debug!("received text from peer (hash {h:x})");
                            }
                        }
                        Err(e) => warn!("bad text frame: {e}"),
                    },
                    // 0x10 FILE_OFFER：清单交 Windows owner 线程上剪贴板
                    0x10 => {
                        #[cfg(windows)]
                        match clipsync::proto::Manifest::from_json_bytes(&payload) {
                            Ok(m) => match m.validate() {
                                // 控制通道无认证，恶意/MITM 可注入伪造 offer；relpath 校验挡住
                                // 「粘贴到投放目录之外」的任意写入原语，条目上限防超大 offer。
                                Err(why) => warn!("rejecting FILE_OFFER: {why}"),
                                Ok(()) => {
                                    info!(
                                        "received FILE_OFFER: session {}, {} entries",
                                        m.session_id,
                                        m.entries.len()
                                    );
                                    if file_sink.send(OwnerMsg::SetOffer(m)).is_err() {
                                        warn!("owner thread gone; cannot set file offer");
                                    }
                                }
                            },
                            Err(e) => warn!("bad FILE_OFFER manifest: {e}"),
                        }
                        #[cfg(not(windows))]
                        {
                            let _ = &payload;
                            debug!("ignoring FILE_OFFER on non-windows peer");
                        }
                    }
                    // 0x11 FILE_REVOKE：清空 Windows 剪贴板文件 offer
                    0x11 => {
                        #[cfg(windows)]
                        {
                            if file_sink.send(OwnerMsg::Clear).is_err() {
                                warn!("owner thread gone; cannot clear file offer");
                            }
                        }
                        #[cfg(not(windows))]
                        debug!("ignoring FILE_REVOKE on non-windows peer");
                    }
                    other => warn!(
                        "unknown frame type 0x{other:02x} ({} bytes) ignored",
                        payload.len()
                    ),
                }
            }
        }
    }
    Ok(())
}
