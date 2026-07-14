//! 文件传输模块。
//!
//! 跨平台纯逻辑 `manifest`；数据服务 `data_server` 用 tokio + std::fs，
//! 亦跨平台（Linux 侧运行、任意平台可跑集成测试），故不 `#[cfg(unix)]` 门控。
//!
//! - `data_object`（`#[cfg(windows)]`）：把清单打包成 `FILEGROUPDESCRIPTORW`
//!   字节，供 Windows 侧 IDataObject 延迟渲染使用。
//! - `net_stream`：`FilePuller` 内核跨平台（阻塞 TCP 客户端），后续的 COM
//!   `IStream` 包装在该文件内 `#[cfg(windows)]` 门控。
//! - `detect_x11`（`#[cfg(unix)]`）：Linux 发送侧用 xclip 检测 X11
//!   `text/uri-list`、解析 `file://` 本地路径、内容去重 hash。
//!
//! 其余平台子模块（clipboard_owner）为 Windows 接收侧。

#[cfg(windows)]
pub mod clipboard_owner;
#[cfg(windows)]
pub mod data_object;
pub mod data_server;
#[cfg(unix)]
pub mod detect_x11;
pub mod manifest;
pub mod net_stream;
