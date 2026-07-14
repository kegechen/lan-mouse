//! clipsync 库：跨平台纯逻辑与平台细节模块。
//!
//! - `clipboard_text`：文本剪贴板读写（从 main.rs 抽出，行为不变）
//! - `proto`：线协议（manifest / 控制帧类型 / 数据握手 + HMAC）
//! - `file`：文件传输（manifest 遍历为跨平台纯逻辑；平台子模块后续阶段加入）
//!
//! 二进制 `main.rs` 通过 `use clipsync::...` 复用这里的 `pub` 项；单元测试跑
//! `cargo test --lib`。将模块放在库目标里而非 binary，可让尚未接线的 `proto`/
//! `file` 保持 `pub`（即公共 API），不产生 dead_code 警告。

pub mod clipboard_text;
pub mod file;
pub mod proto;
