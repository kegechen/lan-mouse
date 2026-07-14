//! 数据连接的**客户端**：`FilePuller` —— 阻塞 `std::net::TcpStream` 上的
//! 按需拉取器。
//!
//! 本文件的 `FilePuller` 内核**跨平台**（纯 std 网络 + `crate::proto`），可在
//! 任意平台单测（测试起 `crate::file::data_server` 真服务对拉）。后续 COM 侧
//! 的 `IStream` 包装（`NetStream`）会**放在本文件、门控 `#[cfg(windows)]`**，
//! 内部持有一个 `FilePuller` 并在 `Read`/`Seek` 时转调 `read_chunk`/`seek_to`。
//!
//! 线上时序（对端见 `data_server`）：
//! 1. `ensure()` 连上目标 → 发 53 字节 `DataReq::encode` 握手
//! 2. 读单字节 status：非 0 → 报错（stale/缺文件/打开失败）
//! 3. status=0 后，裸流即为从 `offset` 起的文件字节，`read_chunk` 逐块读到 EOF
//!
//! `seek_to` 是"真 seek"：丢弃当前连接、只改 `offset`，下次 `read_chunk`
//! 重新握手从新 offset 拉（服务端按 offset seek 文件）。

use crate::proto::DataReq;
use std::io::{Read, Write};
use std::net::TcpStream;

/// 阻塞 TCP 上的单文件按需拉取器。
///
/// 惰性连接：构造时不连网，首个 `read_chunk` 才 `ensure()` 建连并握手。
pub struct FilePuller {
    target: String, // "host:data_port"
    key: Vec<u8>,
    session_id: u32,
    file_id: u32,
    offset: u64,
    sock: Option<TcpStream>,
}

impl FilePuller {
    pub fn new(target: String, key: Vec<u8>, session_id: u32, file_id: u32) -> Self {
        Self {
            target,
            key,
            session_id,
            file_id,
            offset: 0,
            sock: None,
        }
    }

    /// 当前读游标（下一个 `read_chunk` 将返回的首字节在文件中的绝对偏移）。
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// 惰性建连 + 握手。已有连接则直接返回。
    fn ensure(&mut self) -> std::io::Result<()> {
        if self.sock.is_some() {
            return Ok(());
        }
        let mut s = TcpStream::connect(&self.target)?;
        s.set_nodelay(true).ok();
        // 读/写超时：服务端中途停住不发也不断时，避免 Explorer 拷贝线程永久阻塞。
        // 超时 → read/write Err → read_chunk 上抛 → IStream::Read 返 STG_E_READFAULT。
        s.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
        s.set_write_timeout(Some(std::time::Duration::from_secs(30))).ok();
        let req = DataReq {
            session_id: self.session_id,
            file_id: self.file_id,
            offset: self.offset,
        };
        s.write_all(&req.encode(&self.key))?;
        let mut status = [0u8; 1];
        s.read_exact(&mut status)?;
        if status[0] != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("server status {}", status[0]),
            ));
        }
        self.sock = Some(s);
        Ok(())
    }

    /// 读 ≤`buf.len()` 字节，返回实际读到的字节数；EOF 返回 0。
    ///
    /// 首次调用会自动建连握手；读到的字节推进内部 `offset`。
    pub fn read_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.ensure()?;
        let n = self.sock.as_mut().unwrap().read(buf)?;
        self.offset += n as u64;
        Ok(n)
    }

    /// 真 seek：关掉旧连接、置新 `offset`，下次 `read_chunk` 从新位置重开。
    /// 若新 offset 等于当前（如 `Seek(0, CUR)` 查位置的惯用法），不动连接。
    pub fn seek_to(&mut self, new_offset: u64) {
        if new_offset == self.offset {
            return;
        }
        self.sock = None;
        self.offset = new_offset;
    }
}

// ===========================================================================
// COM IStream 包装（#[cfg(windows)]）
//
// 把一个阻塞 `FilePuller` 暴露为 COM `IStream`，供 `data_object` 的
// `CFSTR_FILECONTENTS`(TYMED_ISTREAM) 交给 Explorer 复制引擎。引擎会对该
// 流反复 `Read` 直到 EOF；`Seek` 走"真 seek"（丢连接重握手）。`Drop` 时
// 内部 `FilePuller` 的 `TcpStream` 随之关闭 = 取消当前拉取。
//
// 方法签名以锁定的 `windows 0.62.2` 为准：写下最佳猜测后靠 `cargo build`
// 的 E0053 打印期望签名逐一对齐。
// ===========================================================================

#[cfg(windows)]
use windows::core::implement;
#[cfg(windows)]
use windows::Win32::Foundation::{E_NOTIMPL, S_FALSE, S_OK, STG_E_READFAULT};
#[cfg(windows)]
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STREAM_SEEK,
};

/// COM `IStream`，内部持有一个阻塞 `FilePuller`。COM 方法均 `&self`，故用
/// `Mutex` 提供内部可变性；`size` 供 `Stat`/`Seek(END)` 用。
#[cfg(windows)]
#[implement(IStream)]
pub struct NetStream {
    inner: std::sync::Mutex<FilePuller>,
    size: u64,
}

/// 构造一个可交给 Explorer 的 `IStream`（TYMED_ISTREAM）。
#[cfg(windows)]
pub fn new_com_stream(
    target: String,
    key: Vec<u8>,
    session_id: u32,
    file_id: u32,
    size: u64,
) -> IStream {
    NetStream {
        inner: std::sync::Mutex::new(FilePuller::new(target, key, session_id, file_id)),
        size,
    }
    .into()
}

#[cfg(windows)]
impl ISequentialStream_Impl for NetStream_Impl {
    fn Read(
        &self,
        pv: *mut core::ffi::c_void,
        cb: u32,
        pcbread: *mut u32,
    ) -> windows::core::HRESULT {
        if pv.is_null() {
            return windows::Win32::Foundation::E_POINTER;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let mut puller = self.inner.lock().unwrap();
        match puller.read_chunk(buf) {
            Ok(n) => {
                if !pcbread.is_null() {
                    unsafe { *pcbread = n as u32 };
                }
                if n == 0 {
                    // premature EOF：offset 未达 size 就读到 0（对端 FIN/中断）→ 报错，
                    // 别让 Explorer 把截断的半个文件当成功复制（数据完整性）。
                    if puller.offset() < self.size {
                        return STG_E_READFAULT;
                    }
                    S_FALSE
                } else {
                    S_OK
                }
            }
            Err(_) => {
                if !pcbread.is_null() {
                    unsafe { *pcbread = 0 };
                }
                STG_E_READFAULT
            }
        }
    }

    fn Write(
        &self,
        _pv: *const core::ffi::c_void,
        _cb: u32,
        _pcbwritten: *mut u32,
    ) -> windows::core::HRESULT {
        windows::Win32::Foundation::STG_E_ACCESSDENIED
    }
}

#[cfg(windows)]
impl IStream_Impl for NetStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let mut puller = self.inner.lock().unwrap();
        let new_pos: u64 = match dworigin.0 {
            0 => dlibmove.max(0) as u64,                             // STREAM_SEEK_SET
            1 => (puller.offset() as i64 + dlibmove).max(0) as u64,  // STREAM_SEEK_CUR
            2 => (self.size as i64 + dlibmove).max(0) as u64,        // STREAM_SEEK_END
            _ => return Err(windows::Win32::Foundation::E_INVALIDARG.into()),
        };
        puller.seek_to(new_pos);
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = new_pos };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(windows::Win32::Foundation::E_POINTER.into());
        }
        unsafe {
            let st = &mut *pstatstg;
            *st = std::mem::zeroed();
            st.cbSize = self.size;
            st.r#type = 2; // STGTY_STREAM
            // pwcsName 保持 NULL：STATFLAG_DEFAULT 严格语义要求 CoTaskMemAlloc 填写名字，
            // 但 Explorer 复制引擎实测容忍 NULL，故省略以避免跨模块内存所有权问题。
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

// ===========================================================================
// Tests：起真实 data_server（localhost + 临时文件）对拉，校验全量与 seek 后正确
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::data_server::{serve, ServerState};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// 在 127.0.0.1:0 起一个 data_server，返回其端口。
    async fn spawn_data_server(
        path: std::path::PathBuf,
        session: u32,
        file_id: u32,
        key: Vec<u8>,
    ) -> u16 {
        let mut paths = HashMap::new();
        paths.insert(file_id, path);
        let state = Arc::new(Mutex::new(ServerState {
            session_id: session,
            paths,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve(listener, state, key));
        port
    }

    /// 用阻塞 FilePuller 从头读到 EOF，返回读到的全部字节。
    fn drain(mut puller: FilePuller) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = puller.read_chunk(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_puller_reads_full_and_after_seek() {
        // 造一个跨多个 pump 块的文件（非平凡长度，含全字节值分布）。
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("data.bin");
        let content: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&f, &content).unwrap();

        let key = b"shared-secret".to_vec();
        let session = 5u32;
        let file_id = 1u32;
        let port = spawn_data_server(f.clone(), session, file_id, key.clone()).await;
        let target = format!("127.0.0.1:{}", port);

        // 全量读：FilePuller 从 offset 0 拉，应等于源字节。
        let target_full = target.clone();
        let key_full = key.clone();
        let got_full = tokio::task::spawn_blocking(move || {
            drain(FilePuller::new(target_full, key_full, session, file_id))
        })
        .await
        .unwrap();
        assert_eq!(got_full.len(), content.len());
        assert_eq!(got_full, content);

        // seek 后读：从 offset=12_345 拉，应等于源的对应尾部。
        let seek_off = 12_345usize;
        let got_seek = tokio::task::spawn_blocking(move || {
            let mut p = FilePuller::new(target, key, session, file_id);
            p.seek_to(seek_off as u64);
            assert_eq!(p.offset(), seek_off as u64);
            drain(p)
        })
        .await
        .unwrap();
        assert_eq!(got_seek, content[seek_off..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_puller_errors_on_bad_key() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.bin");
        std::fs::write(&f, b"0123456789").unwrap();
        let key = b"right-key".to_vec();
        let port = spawn_data_server(f, 7, 1, key).await;
        let target = format!("127.0.0.1:{}", port);

        // 错误 key：握手被静默断连，read_exact(status) 读到 EOF → Err。
        let err = tokio::task::spawn_blocking(move || {
            let mut p = FilePuller::new(target, b"wrong-key".to_vec(), 7, 1);
            let mut buf = [0u8; 16];
            p.read_chunk(&mut buf)
        })
        .await
        .unwrap();
        assert!(err.is_err());
    }
}

// ===========================================================================
// COM 端到端测试（#[cfg(all(windows, test))]）
//
// 直接对 new_file_data_object 的 IDataObject 调 GetData(FILECONTENTS) 拿 IStream
// 并读到 EOF，比对源字节 —— 覆盖整条 money 路径：
//   IDataObject::GetData → new_com_stream(NetStream) → IStream::Read
//     → FilePuller → data_server → 文件字节
// 不经 OleSetClipboard/OleGetClipboard（那是 Windows 剪贴板封送、非本模块逻辑）。
// ===========================================================================

#[cfg(all(windows, test))]
mod com_e2e {
    use crate::file::data_object::new_file_data_object;
    use crate::file::data_server::{serve, ServerState};
    use crate::proto::{Entry, Manifest};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use windows::core::w;
    use windows::Win32::Foundation::{S_FALSE, S_OK};
    use windows::Win32::System::Com::{IDataObject, IStream, DVASPECT_CONTENT, FORMATETC, TYMED_ISTREAM};
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;

    async fn spawn_server(path: std::path::PathBuf, session: u32, file_id: u32, key: Vec<u8>) -> u16 {
        let mut paths = HashMap::new();
        paths.insert(file_id, path);
        let state = Arc::new(Mutex::new(ServerState { session_id: session, paths }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve(listener, state, key));
        port
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idataobject_filecontents_streams_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("report.bin");
        // 跨多个 pump 块、含全字节分布的非平凡内容
        let content: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&f, &content).unwrap();

        let key = b"e2e-secret".to_vec();
        let (session, file_id) = (11u32, 0u32);
        let port = spawn_server(f.clone(), session, file_id, key.clone()).await;
        let target = format!("127.0.0.1:{}", port);

        let manifest = Manifest {
            session_id: session,
            entries: vec![Entry {
                file_id,
                relpath: "report.bin".into(),
                size: content.len() as u64,
                is_dir: false,
            }],
        };

        let expected = content.clone();
        let got = tokio::task::spawn_blocking(move || {
            let dobj: IDataObject = new_file_data_object(manifest, target, key);
            let cf_contents = unsafe { RegisterClipboardFormatW(w!("FileContents")) } as u16;
            let fmt = FORMATETC {
                cfFormat: cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
            };
            let med = unsafe { dobj.GetData(&fmt) }.expect("GetData(FILECONTENTS) failed");
            // 从 STGMEDIUM 取出 IStream（TYMED_ISTREAM → u.pstm）
            let stream: IStream = unsafe { (*med.u.pstm).as_ref() }
                .expect("pstm was null")
                .clone();
            // 读到 EOF
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut nread = 0u32;
                let hr = unsafe {
                    stream.Read(buf.as_mut_ptr() as *mut _, buf.len() as u32, Some(&mut nread))
                };
                assert!(hr == S_OK || hr == S_FALSE, "IStream::Read hr={hr:?}");
                if nread == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..nread as usize]);
            }
            out
        })
        .await
        .unwrap();

        assert_eq!(got.len(), expected.len(), "streamed length mismatch");
        assert_eq!(got, expected, "streamed bytes mismatch");
    }
}
