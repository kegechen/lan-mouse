//! Windows 剪贴板 `FILEGROUPDESCRIPTORW` 字节构建（延迟渲染的文件清单）。
//!
//! 本文件只做**纯字节布局**：把 `crate::proto::Manifest` 打包成 Win32
//! `FILEGROUPDESCRIPTORW`（`UINT cItems` + `FILEDESCRIPTORW[cItems]`）的
//! 内存镜像。这样可在不接触真实剪贴板 / COM 的前提下单测布局正确性。
//!
//! 用 `#[repr(C)]` 手工镜像 `FILEDESCRIPTORW`（592 字节），而非依赖
//! `windows` crate 的结构体——避免不同版本对齐/字段差异，且让本模块的
//! 布局断言自洽（IDataObject 侧 GetData 时把本 blob 原样拷进 HGLOBAL）。
//!
//! 字段偏移（Win32 定义，小端）：
//! ```text
//!   +0   dwFlags           u32
//!   +4   clsid             CLSID(16)
//!   +20  sizel             SIZEL  = [i32;2]
//!   +28  pointl            POINTL = [i32;2]
//!   +36  dwFileAttributes  u32
//!   +40  ftCreationTime    FILETIME(8)
//!   +48  ftLastAccessTime  FILETIME(8)
//!   +56  ftLastWriteTime   FILETIME(8)
//!   +64  nFileSizeHigh     u32
//!   +68  nFileSizeLow      u32
//!   +72  cFileName[260]    WCHAR = [u16;260]  (520 bytes)
//!   = 592 bytes
//! ```

/// `FILEDESCRIPTORW` 的 repr(C) 镜像（592 字节）。
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

// FD_* 标志位（哪些字段有效）与文件属性常量。
const FD_ATTRIBUTES: u32 = 0x04;
const FD_FILESIZE: u32 = 0x40;
const FD_PROGRESSUI: u32 = 0x4000;
const FD_UNICODE: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// 把清单打包成 `FILEGROUPDESCRIPTORW` 字节：
/// `[u32 cItems][FILEDESCRIPTORW; cItems]`（本机字节序）。
///
/// - 目录条目：设 `FILE_ATTRIBUTE_DIRECTORY`，大小填 0。
/// - 文件条目：设 `FILE_ATTRIBUTE_NORMAL` + 拆分 64 位大小到 high/low。
/// - 文件名：`relpath` 的正斜杠转反斜杠、UTF-16、截断到 259 WCHAR 留 NUL。
pub fn build_file_group_descriptor(m: &crate::proto::Manifest) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + m.entries.len() * std::mem::size_of::<FileDescriptorW>());
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
        let name: Vec<u16> = e
            .relpath
            .replace('/', "\\")
            .encode_utf16()
            .take(259)
            .collect();
        fd.file_name[..name.len()].copy_from_slice(&name);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &fd as *const _ as *const u8,
                std::mem::size_of::<FileDescriptorW>(),
            )
        };
        out.extend_from_slice(bytes);
    }
    out
}

// ===========================================================================
// COM FileDataObject（#[cfg(windows)]）
//
// 异步 `IDataObject`（延迟渲染）+ `IDataObjectAsyncCapability`。三格式：
//   - CFSTR_FILEDESCRIPTORW  → HGLOBAL 装 build_file_group_descriptor 字节
//   - CFSTR_FILECONTENTS     → 按 lindex 取文件条目 → NetStream IStream(TYMED_ISTREAM)
//   - CFSTR_PREFERREDDROPEFFECT → HGLOBAL 装 DWORD=DROPEFFECT_COPY(1)
//
// 签名以锁定 `windows 0.62.2` 为准：写下最佳猜测靠 cargo build 的 E0053/E0560
// 逐一对齐。
// ===========================================================================

#[cfg(windows)]
mod com {
    use super::build_file_group_descriptor;
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::{implement, w, BOOL, Ref, HRESULT};
    use windows::Win32::Foundation::{
        DV_E_FORMATETC, DV_E_LINDEX, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK,
    };
    use windows::Win32::System::Com::{
        IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
        IEnumSTATDATA, IStream, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
        TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::Shell::{
        IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl,
    };

    const DROPEFFECT_COPY: u32 = 1;

    /// 一个可交给 `OleSetClipboard` 的异步文件 `IDataObject`。
    ///
    /// 复制那刻零传输：Explorer 粘贴时才对每个文件条目 `GetData(FILECONTENTS)`
    /// 拿 `IStream`，按需从数据端口拉字节。
    #[implement(IDataObject, IDataObjectAsyncCapability)]
    pub struct FileDataObject {
        manifest: crate::proto::Manifest,
        target: String,
        key: Vec<u8>,
        async_mode: AtomicBool,
        operating: AtomicBool,
        cf_descriptor: u16,
        cf_contents: u16,
        cf_dropeffect: u16,
    }

    /// 构造 `IDataObject`：注册三个剪贴板格式并缓存其 id。
    pub fn new_file_data_object(
        manifest: crate::proto::Manifest,
        target: String,
        key: Vec<u8>,
    ) -> IDataObject {
        let cf_descriptor = unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) } as u16;
        let cf_contents = unsafe { RegisterClipboardFormatW(w!("FileContents")) } as u16;
        let cf_dropeffect = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) } as u16;
        FileDataObject {
            manifest,
            target,
            key,
            async_mode: AtomicBool::new(false),
            operating: AtomicBool::new(false),
            cf_descriptor,
            cf_contents,
            cf_dropeffect,
        }
        .into()
    }

    impl FileDataObject {
        /// 三格式的 FORMATETC 列表（供 QueryGetData/EnumFormatEtc）。
        fn formats(&self) -> Vec<FORMATETC> {
            vec![
                make_formatetc(self.cf_descriptor, TYMED_HGLOBAL.0 as u32, -1),
                make_formatetc(self.cf_contents, TYMED_ISTREAM.0 as u32, -1),
                make_formatetc(self.cf_dropeffect, TYMED_HGLOBAL.0 as u32, -1),
            ]
        }
    }

    fn make_formatetc(cf: u16, tymed: u32, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex,
            tymed,
        }
    }

    /// 把字节拷入一个 moveable HGLOBAL。
    unsafe fn hglobal_from_bytes(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let dst = GlobalLock(h);
        if dst.is_null() {
            return Err(windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }

    fn stgmedium_hglobal(h: HGLOBAL) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    fn stgmedium_istream(stream: IStream) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    impl IDataObject_Impl for FileDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            if pformatetcin.is_null() {
                return Err(windows::Win32::Foundation::E_INVALIDARG.into());
            }
            let fmt = unsafe { &*pformatetcin };
            let cf = fmt.cfFormat;
            if cf == self.cf_descriptor {
                let blob = build_file_group_descriptor(&self.manifest);
                let h = unsafe { hglobal_from_bytes(&blob)? };
                Ok(stgmedium_hglobal(h))
            } else if cf == self.cf_dropeffect {
                let h = unsafe { hglobal_from_bytes(&DROPEFFECT_COPY.to_ne_bytes())? };
                Ok(stgmedium_hglobal(h))
            } else if cf == self.cf_contents {
                let idx = fmt.lindex;
                if idx < 0 {
                    return Err(DV_E_LINDEX.into());
                }
                let entry = match self.manifest.entries.get(idx as usize) {
                    Some(e) if !e.is_dir => e,
                    Some(_) => return Err(DV_E_FORMATETC.into()),
                    None => return Err(DV_E_LINDEX.into()),
                };
                let stream = crate::file::net_stream::new_com_stream(
                    self.target.clone(),
                    self.key.clone(),
                    self.manifest.session_id,
                    entry.file_id,
                    entry.size,
                );
                Ok(stgmedium_istream(stream))
            } else {
                Err(DV_E_FORMATETC.into())
            }
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            if pformatetc.is_null() {
                return DV_E_FORMATETC;
            }
            let fmt = unsafe { &*pformatetc };
            let cf = fmt.cfFormat;
            if cf == self.cf_descriptor || cf == self.cf_contents || cf == self.cf_dropeffect {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            _pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            E_NOTIMPL
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection == 1 {
                // DATADIR_GET
                Ok(FormatEnumerator {
                    formats: self.formats(),
                    pos: std::sync::Mutex::new(0),
                }
                .into())
            } else {
                Err(E_NOTIMPL.into())
            }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    impl IDataObjectAsyncCapability_Impl for FileDataObject_Impl {
        fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
            self.async_mode.store(fdoopasync.as_bool(), Ordering::SeqCst);
            Ok(())
        }

        fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
            Ok(self.async_mode.load(Ordering::SeqCst).into())
        }

        fn StartOperation(&self, _pbcreserved: Ref<IBindCtx>) -> windows::core::Result<()> {
            self.operating.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn InOperation(&self) -> windows::core::Result<BOOL> {
            Ok(self.operating.load(Ordering::SeqCst).into())
        }

        fn EndOperation(
            &self,
            _hresult: HRESULT,
            _pbcreserved: Ref<IBindCtx>,
            _dweffects: u32,
        ) -> windows::core::Result<()> {
            self.operating.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 极简 `IEnumFORMATETC`：一个 `Vec<FORMATETC>` + 游标。
    #[implement(IEnumFORMATETC)]
    struct FormatEnumerator {
        formats: Vec<FORMATETC>,
        pos: std::sync::Mutex<usize>,
    }

    impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
        fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
            let mut pos = self.pos.lock().unwrap();
            let mut fetched = 0u32;
            while fetched < celt && *pos < self.formats.len() {
                unsafe { *rgelt.add(fetched as usize) = self.formats[*pos] };
                *pos += 1;
                fetched += 1;
            }
            if !pceltfetched.is_null() {
                unsafe { *pceltfetched = fetched };
            }
            if fetched == celt {
                S_OK
            } else {
                S_FALSE
            }
        }

        fn Skip(&self, celt: u32) -> windows::core::Result<()> {
            let mut pos = self.pos.lock().unwrap();
            *pos = (*pos + celt as usize).min(self.formats.len());
            Ok(())
        }

        fn Reset(&self) -> windows::core::Result<()> {
            *self.pos.lock().unwrap() = 0;
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
            let pos = *self.pos.lock().unwrap();
            Ok(FormatEnumerator {
                formats: self.formats.clone(),
                pos: std::sync::Mutex::new(pos),
            }
            .into())
        }
    }
}

#[cfg(windows)]
pub use com::new_file_data_object;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Entry, Manifest};

    /// 每个 `FILEDESCRIPTORW` 的字节大小（用于按下标定位）。
    const FD_SIZE: usize = std::mem::size_of::<FileDescriptorW>();
    /// blob 前 4 字节是 cItems，条目从 offset 4 起。
    const HEADER: usize = 4;

    /// 读第 i 个条目的 `cFileName`（UTF-16，至 NUL 终止）为 String。
    fn read_cfilename(blob: &[u8], i: usize) -> String {
        let base = HEADER + i * FD_SIZE + 72; // cFileName 在描述符内偏移 72
        let mut units = Vec::new();
        let mut off = base;
        for _ in 0..260 {
            let u = u16::from_ne_bytes([blob[off], blob[off + 1]]);
            if u == 0 {
                break;
            }
            units.push(u);
            off += 2;
        }
        String::from_utf16(&units).unwrap()
    }

    /// 第 i 个条目是否带 `FILE_ATTRIBUTE_DIRECTORY`。
    fn entry_is_dir(blob: &[u8], i: usize) -> bool {
        let base = HEADER + i * FD_SIZE + 36; // dwFileAttributes 偏移 36
        let attr = u32::from_ne_bytes(blob[base..base + 4].try_into().unwrap());
        attr & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// 第 i 个条目的 64 位文件大小（high<<32 | low）。
    fn entry_size(blob: &[u8], i: usize) -> u64 {
        let hi_off = HEADER + i * FD_SIZE + 64; // nFileSizeHigh 偏移 64
        let lo_off = HEADER + i * FD_SIZE + 68; // nFileSizeLow  偏移 68
        let hi = u32::from_ne_bytes(blob[hi_off..hi_off + 4].try_into().unwrap()) as u64;
        let lo = u32::from_ne_bytes(blob[lo_off..lo_off + 4].try_into().unwrap()) as u64;
        (hi << 32) | lo
    }

    /// 第 i 个条目的 dwFlags。
    fn entry_flags(blob: &[u8], i: usize) -> u32 {
        let base = HEADER + i * FD_SIZE; // dwFlags 偏移 0
        u32::from_ne_bytes(blob[base..base + 4].try_into().unwrap())
    }

    #[test]
    fn filedescriptorw_is_592_bytes() {
        // 若这条失败，说明 repr(C) 布局与 Win32 定义不符，其余偏移断言全部失效。
        assert_eq!(FD_SIZE, 592);
    }

    #[test]
    fn descriptor_has_correct_count_and_names() {
        let m = Manifest {
            session_id: 1,
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
        let blob = build_file_group_descriptor(&m);

        // 总长度 = 4 + 2*592
        assert_eq!(blob.len(), HEADER + 2 * FD_SIZE);

        // cItems
        let cnt = u32::from_ne_bytes(blob[0..4].try_into().unwrap());
        assert_eq!(cnt, 2);

        // 名字：目录 "d"；第二条 "d\a.txt"（反斜杠）
        assert_eq!(read_cfilename(&blob, 0), "d");
        assert_eq!(read_cfilename(&blob, 1), r"d\a.txt");

        // 目录/文件属性
        assert!(entry_is_dir(&blob, 0));
        assert!(!entry_is_dir(&blob, 1));

        // 大小：目录 0，文件 5
        assert_eq!(entry_size(&blob, 0), 0);
        assert_eq!(entry_size(&blob, 1), 5);

        // flags 至少带 UNICODE | FILESIZE | ATTRIBUTES | PROGRESSUI
        let want = FD_UNICODE | FD_FILESIZE | FD_ATTRIBUTES | FD_PROGRESSUI;
        assert_eq!(entry_flags(&blob, 0) & want, want);
        assert_eq!(entry_flags(&blob, 1) & want, want);
    }

    #[test]
    fn descriptor_handles_large_size_and_unicode_name() {
        // 大于 4GiB 的大小拆 high/low；中文文件名 UTF-16 正确回读。
        let big: u64 = (5u64 << 32) | 123;
        let m = Manifest {
            session_id: 2,
            entries: vec![Entry {
                file_id: 0,
                relpath: "目录/大文件.bin".into(),
                size: big,
                is_dir: false,
            }],
        };
        let blob = build_file_group_descriptor(&m);
        assert_eq!(entry_size(&blob, 0), big);
        assert_eq!(read_cfilename(&blob, 0), r"目录\大文件.bin");
    }

    #[test]
    fn empty_manifest_is_just_count_zero() {
        let m = Manifest {
            session_id: 0,
            entries: vec![],
        };
        let blob = build_file_group_descriptor(&m);
        assert_eq!(blob.len(), HEADER);
        assert_eq!(u32::from_ne_bytes(blob[0..4].try_into().unwrap()), 0);
    }
}
