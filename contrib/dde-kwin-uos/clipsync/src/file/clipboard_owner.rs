//! Windows 剪贴板 owner：专用 STA 线程 + OLE 剪贴板 + 消息泵。
//!
//! `OleSetClipboard` 要求调用线程是 STA 且**持续泵消息**（否则 OLE 剪贴板
//! 数据封送会死锁）。因此把整个 owner 放到一条专用 OS 线程：
//!   1. `OleInitialize`（隐含 `CoInitialize(APARTMENTTHREADED)` → STA）
//!   2. 循环：先排空 `OwnerMsg`，再 `MsgWaitForMultipleObjectsEx` 等消息/超时，
//!      然后 `PeekMessageW`/`TranslateMessage`/`DispatchMessageW` 泵掉消息
//!   3. `SetOffer` → `OleSetClipboard(FileDataObject)`；`Clear` → 清空剪贴板
//!   4. 通道断开或收到 `WM_QUIT` → 退出前 `OleUninitialize`
//!
//! `rx` 无可等待 HANDLE，故用 200ms 超时轮询它；消息一到 `MsgWaitForMultipleObjectsEx`
//! 立即返回，OLE 封送不受轮询延迟影响（offer 罕见，轮询开销可忽略）。

use crate::proto::Manifest;
use std::sync::mpsc::{Receiver, TryRecvError};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard, OleUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage,
    MWMO_INPUTAVAILABLE, MSG, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
};

/// owner 线程的控制消息（从控制侧 `handle_connection` 送来）。
pub enum OwnerMsg {
    /// 收到 FILE_OFFER：把清单作为延迟渲染的 `IDataObject` 放上剪贴板。
    SetOffer(Manifest),
    /// 收到 FILE_REVOKE：清空剪贴板。
    Clear,
}

/// 在专用 OS 线程上运行 owner；`target` 为 "host:data_port"，`key` 为共享密钥。
///
/// 本函数会阻塞该线程直到通道断开或收到 `WM_QUIT`。
pub fn run_owner(rx: Receiver<OwnerMsg>, target: String, key: Vec<u8>) {
    unsafe {
        if let Err(e) = OleInitialize(None) {
            log::error!("OleInitialize failed: {e:?}; clipboard owner not started");
            return;
        }
    }
    log::info!("clipboard owner thread started (STA); data target = {target}");

    'pump: loop {
        // 1) 排空所有待办 OwnerMsg。
        loop {
            match rx.try_recv() {
                Ok(OwnerMsg::SetOffer(manifest)) => {
                    let n = manifest.entries.len();
                    let session = manifest.session_id;
                    let obj: IDataObject = crate::file::data_object::new_file_data_object(
                        manifest,
                        target.clone(),
                        key.clone(),
                    );
                    match unsafe { OleSetClipboard(&obj) } {
                        Ok(()) => {
                            log::info!("file offer on clipboard: session {session}, {n} entries")
                        }
                        Err(e) => log::warn!("OleSetClipboard(offer) failed: {e:?}"),
                    }
                }
                Ok(OwnerMsg::Clear) => {
                    match unsafe { OleSetClipboard(None::<&IDataObject>) } {
                        Ok(()) => log::info!("clipboard file offer cleared"),
                        Err(e) => log::warn!("OleSetClipboard(clear) failed: {e:?}"),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::info!("owner channel disconnected; clipboard owner exiting");
                    break 'pump;
                }
            }
        }

        // 2) 等窗口消息或 200ms 超时（用于回来轮询 rx）。
        unsafe {
            MsgWaitForMultipleObjectsEx(None, 200, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }

        // 3) 泵掉队列里所有消息（OLE 剪贴板封送依赖此循环）。
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break 'pump;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe { OleUninitialize() };
    log::info!("clipboard owner thread stopped");
}
