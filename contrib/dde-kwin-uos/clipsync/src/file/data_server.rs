//! 数据服务（tokio）：独立数据连接的服务端。
//!
//! 跨平台（tokio + std::fs）：Linux 侧运行以按 offset 流式回传文件字节，
//! 集成测试在任意平台（含 Windows）跑得通。握手认证走 HMAC-SHA256
//! （见 `crate::proto::DataReq`），并校验 session_id 防旧会话串读。
//!
//! 线上时序：
//! 1. 客户端连上 → 发 53 字节握手 `DataReq::encode`
//! 2. 服务端 `decode_and_verify`：HMAC 不过 → 静默断连（不回 status）
//! 3. session/file 校验：不匹配 → 回单字节非零 status 后断
//! 4. OK → 回 status=0，然后从 `offset` 起用固定 256KB 缓冲 pump 到 EOF

use crate::proto::DataReq;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// 数据服务共享状态：当前有效 session 及其 file_id → 绝对路径映射。
///
/// 与控制侧共享（`Arc<Mutex<..>>`）：复制新一组文件时递增 `session_id`
/// 并替换 `paths`，旧 session 的请求随即被拒（stale）。
pub struct ServerState {
    pub session_id: u32,
    pub paths: HashMap<u32, PathBuf>,
}

/// 接受数据连接的主循环：每个连接派生一个 task 走 `handle`。
pub async fn serve(listener: TcpListener, state: Arc<Mutex<ServerState>>, key: Vec<u8>) {
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // accept 持续失败（如 FD 耗尽）时退避，避免 100% CPU 空转
                log::warn!("data accept error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, state, key).await {
                log::debug!("data conn ended: {e}");
            }
        });
    }
}

async fn handle(
    mut sock: TcpStream,
    state: Arc<Mutex<ServerState>>,
    key: Vec<u8>,
) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    let mut hs = [0u8; 53];
    // 握手读超时：防 slow-loris（连上只发 <53 字节再挂起，永久占住 task/FD）。
    match tokio::time::timeout(std::time::Duration::from_secs(10), sock.read_exact(&mut hs)).await {
        Ok(r) => r?,
        Err(_) => {
            log::warn!("data handshake read timed out");
            return Ok(());
        }
    };
    let req = match DataReq::decode_and_verify(&hs, &key) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("handshake reject: {e}");
            return Ok(()); // 认证失败直接断，不回 status
        }
    };
    // 查 session + file_id
    let path = {
        let st = state.lock().await;
        if st.session_id != req.session_id {
            drop(st);
            let _ = sock.write_u8(1).await;
            return Ok(());
        }
        match st.paths.get(&req.file_id).cloned() {
            Some(p) => p,
            None => {
                drop(st);
                let _ = sock.write_u8(2).await;
                return Ok(());
            }
        }
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => {
            let _ = sock.write_u8(3).await;
            return Ok(());
        }
    };
    if req.offset > 0 {
        file.seek(std::io::SeekFrom::Start(req.offset)).await?;
    }
    sock.write_u8(0).await?; // status OK
                             // 固定缓冲 pump，内存恒定
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sock.write_all(&buf[..n]).await?;
    }
    sock.flush().await?;
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::DataReq;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;

    async fn spawn_server(
        paths: HashMap<u32, std::path::PathBuf>,
        session: u32,
        key: Vec<u8>,
    ) -> u16 {
        let state = Arc::new(Mutex::new(ServerState {
            session_id: session,
            paths,
        }));
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
        let mut paths = HashMap::new();
        paths.insert(1u32, f);
        let key = b"k".to_vec();
        let port = spawn_server(paths, 7, key.clone()).await;

        // 正确请求：从 offset=3 读
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(
            &DataReq {
                session_id: 7,
                file_id: 1,
                offset: 3,
            }
            .encode(&key),
        )
        .await
        .unwrap();
        let status = s.read_u8().await.unwrap();
        assert_eq!(status, 0);
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"3456789");

        // 错误 key：连接被拒（读不到 status=0）
        let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s2.write_all(
            &DataReq {
                session_id: 7,
                file_id: 1,
                offset: 0,
            }
            .encode(b"wrong"),
        )
        .await
        .unwrap();
        assert!(s2.read_u8().await.is_err() || s2.read_u8().await.unwrap() != 0);

        // stale session：拒
        let mut s3 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s3.write_all(
            &DataReq {
                session_id: 999,
                file_id: 1,
                offset: 0,
            }
            .encode(&key),
        )
        .await
        .unwrap();
        assert_ne!(s3.read_u8().await.unwrap(), 0);
    }
}
