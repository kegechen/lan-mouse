# clipsync 文件复制粘贴设计（Linux X11 → Windows）

> 状态：设计已评审通过，待实现。
> 日期：2026-07-14
> 关联：`contrib/dde-kwin-uos/clipsync/src/main.rs`（现有文本同步）、`connect.ps1`（部署）

## 1. 目标与范围

在现有 clipsync（跨机剪贴板文本同步 daemon）基础上，扩展**文件复制粘贴**能力：

> 用户用 lan-mouse 把鼠标/键盘漫游到远端 Linux，在远端文件管理器复制文件/文件夹，右 Alt 漫游回本地 Windows，在桌面等位置直接 `Ctrl+V` 粘贴，文件被传回并落到本地；粘贴时弹出 **Windows Explorer 原生复制进度对话框**，实时反映网络传输进度。

### 已定范围（MVP）

| 维度 | 决定 |
|---|---|
| 方向 | **仅 Linux（远端源）→ Windows（本地目标）单向** |
| 进度体验 | **方案① 全原生**：粘贴时才传字节（delayed rendering），Explorer 自己的进度框 |
| 内容范围 | **多文件 + 递归文件夹 + GB 级大文件真流式** |
| 安全 | **方案 B**：复用 lan-mouse 的 `authentication_key` 做握手认证；字节明文走线 |
| Linux 桌面 | **暂时只支持 X11**（用 xclip 检测 uri-list）；现有文本同步的 wl-paste 路径保持不动 |

### 明确不做（YAGNI）

- 反向（Windows → Linux）文件粘贴。
- Wayland 原生应用复制文件的检测（待 X11 路径联调后再评估）。
- 传输字节加密 / TLS（方案 C）。
- 剪切（cut/move）语义——只做复制（`DROPEFFECT_COPY`）。

## 2. 关键约束与取舍

- **方案① 的硬约束**：Windows 侧必须常驻一个**剪贴板 owner 进程 + STA 线程 + 消息循环**来服务 OLE delayed rendering；`arboard` 做不到，文件路径改用 Win32/OLE COM（`windows` crate），文本路径仍用 arboard。
- **offer 有效期取舍**：一个文件 offer **只在它仍是 Linux 当前剪贴板内容时有效**。用户在远端又复制了别的东西后再回来粘贴旧 offer，会被 data_server 以 stale session 拒绝、该次粘贴干净失败。这符合"你已经复制了别的"的直觉。
- **架构选型（方案 2）**：控制信令走现有 clipsync 那条 TCP；**大文件字节另开专用数据连接**，裸字节流靠 TCP 背压做流控。相比"全挤一条 socket 做多路复用"（方案 1）避免了队头阻塞与手写流控，大文件路径最简单可靠。代价是 Linux 多开一个数据监听端口 + 每条数据连接单独握手。

## 3. 架构

在**现有 clipsync crate 内扩展**，不新建二进制。文本同步逻辑原样保留。

### Windows 侧分两半

- **tokio 侧**：维护 clipsync 控制 TCP，收 `FILE_OFFER`。
- **STA 线程**（专用 OS 线程，非 tokio worker）：`OleInitialize` → 持有 `IDataObject` → `OleSetClipboard` → 跑 `GetMessage` 消息泵服务 OLE。
- 两者用 channel / `PostThreadMessage` 通信。
- **`IStream` 数据拉取用裸阻塞 `std::net::TcpStream`**：Explorer 在自己的后台线程上**同步**调 `IStream::Read`，阻塞 socket I/O 天然契合，Windows 数据路径不需要 tokio。

### Linux 侧

- 控制任务（现有 tokio）+ 新增 `data_server`（tokio 监听）+ X11 检测轮询。

## 4. 端到端流程

```
Linux（源）                          Windows（目标）
─────────                            ─────────
1. 漫游过去，文件管理器复制文件/文件夹
   → X11 CLIPBOARD 提供 text/uri-list
2. clipsync 轮询到 uri-list（不读字节）
   递归展开目录 → manifest：
   [{file_id, relpath, size, is_dir}, ...]
   记住 file_id → 绝对路径 映射
3. ── FILE_OFFER(manifest) 走控制 TCP ──▶
                                     4. STA 线程建 FILEGROUPDESCRIPTORW
                                        每条目一个 FILEDESCRIPTORW
                                        OleSetClipboard(异步 IDataObject)
                                     5. 右 Alt 漫游回来，桌面 Ctrl+V
                                     6. Explorer 读 CFSTR_FILEDESCRIPTOR 拿列表
                                        对每个文件条目读 CFSTR_FILECONTENTS
                                        → 返回一个 IStream
                                     7. IStream::Read 首次被调 →
   ◀── 新开数据 TCP：AUTH + READ(file_id, offset) ──
8. data_server 验 key，裸流 pump 该文件字节 ──▶
                                        字节喂给 Explorer，
                                        原生进度框实时前进
                                     9. 全部流读完 → 文件落地
                                        异步操作 EndOperation
```

**要点**：复制那刻**零字节传输**；字节全在第 7–8 步粘贴时按需拉。目录条目让 Explorer 自动建文件夹；空目录单独一条 `FILE_ATTRIBUTE_DIRECTORY` 条目。

## 5. 协议

### 控制帧（在现有 TLV `[u8 type][u32 BE len][payload]` 上扩展，与文本 `0x01` 共存一条连接）

- `0x10 FILE_OFFER`：payload = JSON manifest
  ```json
  {
    "session_id": 42,
    "entries": [
      {"file_id": 0, "relpath": "myfolder", "size": 0, "is_dir": true},
      {"file_id": 1, "relpath": "myfolder/a.txt", "size": 1234, "is_dir": false},
      {"file_id": 2, "relpath": "myfolder/sub/b.bin", "size": 999999, "is_dir": false}
    ]
  }
  ```
  `relpath` 用正斜杠，Windows 侧转反斜杠。
- `0x11 FILE_REVOKE`（可选）：Linux 剪贴板变化，撤销上一个 offer。

### 数据连接（独立 TCP，Linux 新监听端口，每次 `IStream` 首读时新建一条）

```
Windows→Linux:  [MAGIC "LMFD"][u8 ver]
                [32B HMAC-SHA256(key, session_id ‖ file_id ‖ offset)]
                [u32 session_id][u32 file_id][u64 offset]
Linux→Windows:  [u8 status]      status=0 OK / 非0 错误(文件没了/越权/stale)
                status=0 后：裸字节流，从 offset 到 EOF，TCP 背压控速
```

- 认证：复用 `authentication_key` 做 HMAC，验不过直接断。
- `offset` 支持是为 `IStream::Seek`——Explorer 若 seek，就按新 offset 重开一条数据连接。
- 一条数据连接只服务一个文件；多文件并发 = 多条连接，天然不互相阻塞。

## 6. Linux 侧实现（X11，`#[cfg(unix)]`）

- **`detect_x11`**：轮询 `xclip -selection clipboard -t TARGETS -o`；若含 `text/uri-list` → `xclip -selection clipboard -t text/uri-list -o` 取 URI，percent-decode 解析 `file://` → 本地绝对路径。用 uri-list 的 hash 去重（同现有文本 dedup 套路），只处理复制语义。
  - ⚠️ 待验证：dde-kwin-uos 多为 Xwayland，文件管理器复制的 uri-list 通常 xclip 可读；纯 Wayland 原生应用复制可能读不到，届时再加 wl-paste 路径。
- **`manifest`**：文件 → 一条 `{relpath=basename, size, is_dir:false}`；目录 → **递归遍历**，relpath 含顶层文件夹名（复制 `myfolder` → `myfolder/a.txt`、`myfolder/sub/b.bin`，外加 `myfolder`、`myfolder/sub` 目录条目）。递增分配 `file_id`，存 `file_id → 绝对路径` 到 `Arc<Mutex<HashMap>>`。**不跟随 symlink**（防环），深度/数量做 sanity 上限。建好发 `FILE_OFFER`。
- **`data_server`**（tokio `TcpListener`，新数据端口）：每条连接读握手 → 验 HMAC → 验 `session_id` 为当前 offer（挡 stale）→ 查 `file_id` 取绝对路径 → `open` + `seek(offset)` → 固定缓冲（256KB）循环 pump 到 socket。文件缺失/无权限 → 回 `status!=0` 后关闭。
- **session 失效**：Linux 剪贴板变成新 uri-list 或变成纯文本时 `session_id++`、清映射表（老 session 请求一律拒），可选发 `FILE_REVOKE`。

## 7. Windows 侧实现（COM，`#[cfg(windows)]`）

- **control task（tokio）**：收 `FILE_OFFER` → 把 manifest 经 channel/`PostThreadMessage` 丢给 STA 线程。
- **`clipboard_owner`（专用 std::thread，STA）**：`OleInitialize` → message-only 隐藏窗口 → 收到 manifest 建 `FileDataObject` → `OleSetClipboard(dataobj)` 持活 → `GetMessage` 消息泵。新 offer 到来就重建 + 再 `OleSetClipboard` 替换。
- **`FileDataObject`**（`windows` crate `#[implement]`，实现 `IDataObject` + `IDataObjectAsyncCapability` + `IEnumFORMATETC`）：
  - `GetData(CFSTR_FILEDESCRIPTORW)` → HGLOBAL 装 `FILEGROUPDESCRIPTORW`：`cItems` + 每条 `FILEDESCRIPTORW`（`dwFlags=FD_UNICODE|FD_FILESIZE|FD_ATTRIBUTES|FD_PROGRESSUI`，`nFileSizeLow/High`，目录条目置 `FILE_ATTRIBUTE_DIRECTORY`，`cFileName`=反斜杠 UTF-16 相对路径）。
  - `GetData(CFSTR_FILECONTENTS, lindex=i, TYMED_ISTREAM)` → 为第 i 个**文件**条目返回一个 `NetStream`（目录条目不会被请求内容）。
  - `GetData(CFSTR_PREFERREDDROPEFFECT)` → `DROPEFFECT_COPY`。
  - `IDataObjectAsyncCapability::SetAsyncMode(TRUE)` → Explorer 后台线程做拷贝、进度框流畅；`StartOperation/EndOperation` 感知完成。
  - 三个 format 用 `RegisterClipboardFormatW` 注册。
- **`NetStream`**（实现 `IStream`/`ISequentialStream`，跑在 Explorer 后台线程，**裸阻塞 `std::net::TcpStream`**）：
  - 持 `session_id/file_id/size/offset` + 懒开的数据连接。
  - `Read`：socket 未开则先开 + 握手；从 socket 读 ≤cb 字节拷进 `pv`、推进 offset；网络错 → 返 `E_FAIL`/`STG_E_READFAULT`；EOF 返 `S_FALSE`。
  - `Stat` → `cbSize` = manifest 文件大小（Explorer 据此算总进度）。
  - `Seek` → 真 seek 就关旧连接、置新 offset，下次 Read 按新 offset 重开。
  - **取消**：stream 被释放（用户取消进度框）→ `Drop` 关 socket → Linux pump 发现断连中止。

## 8. 错误处理与边界

| 场景 | 行为 |
|---|---|
| 粘贴中途网络断 | `NetStream::Read` 返 `STG_E_READFAULT` → Explorer 对该文件报错、清理半成品、其余不受影响 |
| 用户取消 Explorer 进度框 | stream 释放 → `NetStream::Drop` 关 socket → Linux pump 中止 |
| 粘贴前 Windows 又复制了别的 | 丢失 OLE 剪贴板 owner，旧 offer 作废（正常语义，无害） |
| 复制后 Linux 剪贴板变了才粘贴（stale） | `session_id` 已 bump → data_server 拒 → 该次粘贴干净失败 |
| 文件复制后被删/改权限 | data_server open 失败 → `status!=0` → 该文件报错，其余继续 |
| 空文件 / 空目录 | 空文件 Read 立即 0(S_FALSE)；空目录只发目录条目、不请求内容 |
| GB 大文件 | 两侧固定缓冲 pump，内存恒定；`Stat` 给大小让进度框算总量 |
| 非 ASCII 文件名 | manifest UTF-8 → `FILEDESCRIPTORW` UTF-16；`/`→`\` 归一 |
| 路径穿越 | Windows 只发 `file_id`（不发路径），路径由 Linux 自己遍历得出，无穿越面；HMAC 覆盖 `session_id‖file_id‖offset`，无 key 直接拒 |

## 9. 测试策略

1. **纯逻辑单测**（两平台，不碰剪贴板/COM）：递归 manifest（relpath/目录条目/跳 symlink）、uri-list 解析（percent-decode）、帧序列化、HMAC 计算/校验。
2. **Linux `data_server` 集成测**：temp 目录树 → 起 server → 客户端握手请求 `file_id+offset` → 断言字节一致；坏 HMAC 拒、stale session 拒、缺文件回 `status!=0`。
3. **Windows COM 路径测（不靠 Explorer）**：进程内"假粘贴器"——`OleGetClipboard` → 枚举 `FILEDESCRIPTOR` → 逐个把 `FILECONTENTS` 的 IStream 读完落盘 → 与源比对。Explorer 无法脚本化，用假粘贴器确定性覆盖整条 COM+网络路径。
4. **大文件测**：多 GB 稀疏文件，验内存恒定 + 正确 + 中途取消能中止。
5. **最终人工验收**：真 Explorer 桌面粘贴，看原生进度框、取消、目录树、中文名。

## 10. 模块落地与依赖

```
clipsync/src/
  main.rs            // 现有 tokio 控制循环 + 文本路径 + 接入文件路径
  proto.rs           // 帧类型(0x01文本/0x10 OFFER/0x11 REVOKE)、manifest(serde)、握手、HMAC
  clipboard_text.rs  // 现有文本读写(从 main 抽出)
  file/manifest.rs   // 递归遍历、file_id 映射、session
  #[cfg(unix)]  detect_x11.rs   // xclip TARGETS+uri-list 轮询、uri 解析
  #[cfg(unix)]  data_server.rs  // tokio 监听、认证、字节 pump
  #[cfg(windows)] clipboard_owner.rs // STA 线程、OleInitialize、消息泵、OleSetClipboard
  #[cfg(windows)] data_object.rs     // FileDataObject: IDataObject+AsyncCapability+EnumFORMATETC+FILEGROUPDESCRIPTOR
  #[cfg(windows)] net_stream.rs      // NetStream: 阻塞 TCP 上的 IStream，握手/read/seek/stat/cancel
```

**新增依赖**：Windows 侧 `windows` crate（`Win32_System_Com` / `Win32_System_Ole` / `Win32_System_DataExchange` / `Win32_System_Memory` / `Win32_Foundation`）；两侧 `hmac` + `sha2`（认证）；文本仍用 arboard；Linux 用 `Command(xclip)`。

## 11. 待验证 / 开放问题

- **Xwayland vs 纯 Wayland**：xclip 能否读到 dde-kwin 环境文件管理器复制的 `text/uri-list`（推断可读，待实测）。
- **数据端口选择与防火墙**：新数据监听端口的具体端口号 + connect.ps1/firewall 规则接入方式（实现时定）。
- **STA COM owner 与现有 tokio 主循环的进程内共存**：clipsync 当前是 `#[tokio::main]` 单文件，需引入专用 STA 线程并理顺关停顺序。
- **`windows` crate 对 `IDataObjectAsyncCapability` / `IEnumFORMATETC` 的 `#[implement]` 支持细节**：实现时对着官方文档核对 vtable 与线程模型。
