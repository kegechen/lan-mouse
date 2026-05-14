# Lan Mouse 使用说明

Windows + Linux 跨设备共用键鼠（Software KVM），基于 patched
[lan-mouse v0.10.0](https://github.com/feschber/lan-mouse) +
自定义 sticky-corners + 半透明边缘 indicator + uinput 注入。

---

## 快速开始

```powershell
cd D:\tools\lan-mouse

# 第一次：进交互向导（4 个问题：SSH 目标 / 方向 / 主机 IP / 端口）
.\connect.ps1

# 之后日常：探到已装已跑，直接重新激活，秒级完成
.\connect.ps1
```

详细帮助：

```powershell
Get-Help .\connect.ps1 -Full
Get-Help .\connect.ps1 -Examples
```

---

## 命令行参数

| 参数 | 默认 | 说明 |
|------|------|------|
| `-Target user@host` | `uos@192.168.137.27` | 远端 SSH 目标。**必须带 `user@`**；传裸 IP 时脚本会用 `-Hostname` 补成 `uos@host` |
| `-Direction left/right/top/bottom` | `left` | 远端机器在 Windows 桌面的哪个方向 |
| `-WinHostIp 192.168.x.x` | `192.168.137.1` | Windows 主机在远端能访问到的 IP（同 WiFi 场景填 Win 的 LAN IP） |
| `-Port 4242` | `4242` | lan-mouse UDP 端口 |
| `-Hostname uos` | `uos` | 远端 hostname（toml 显示用 + 裸 IP 时补 `user@` 的兜底用户名） |
| `-DwellMs N` | `0` | 鼠标到边缘必须停留 N 毫秒才越界（防误触） |
| `-ClipsyncPort N` | `4243` | 剪贴板同步 TCP 端口 |
| `-NoClipsync` | off | 禁用剪贴板同步 daemon |
| `-DemoIndicator` | off | 让 indicator 永久显示（远程截图调试） |
| `-Setup` | — | 强制重跑配置向导 |
| `-Force` | — | 远端强制重新拉源码 + 编译（修复损坏的安装） |
| `-Stop` | — | 停止两端 daemon |

CLI 参数会覆盖 `.connect-config.json` 的值（一次性，不写回）。

### 示例

```powershell
# 最常见：无参，复用上次配置
.\connect.ps1

# Windows 移动热点（ICS）场景
.\connect.ps1 -Target uos@192.168.137.27

# Win 和 UOS 同一个 WiFi（必须带 uos@，且 -WinHostIp 填 Win 的 LAN IP）
.\connect.ps1 -Target uos@10.81.194.113 -WinHostIp 10.81.194.50

# 传裸 IP 也行，会自动补成 uos@10.81.194.113
.\connect.ps1 -Target 10.81.194.113

# UOS 在右边、防误触越界（200ms dwell）
.\connect.ps1 -Direction right -DwellMs 200

# 停掉两端 daemon
.\connect.ps1 -Stop

# 远端重拉源码重编（patch 更新后用）
.\connect.ps1 -Force
```

---

## 行为说明

### 越界

把鼠标向 `-Direction` 方向滑出 Windows 屏幕边缘 → 越界到远端。

- `-DwellMs 0`（默认）：碰边即过
- `-DwellMs 200`：必须在边缘"顶住" 200 ms（鼠标可以在边缘上下滑动也行，鼠标静止也算）
- `-DwellMs 500`：500 ms，更稳但也更钝

### 视觉反馈（Indicator）

鼠标贴边时屏幕边缘会显示一个 8×160 的**半透明小条**（layered window，
alpha ~78%），橙色填充进度条从底向上 / 从左向右随 dwell 累计。
dwell 满足时小条消失（鼠标越界），鼠标拉回内部时小条立即消失。

注：截图工具（`Print Screen`、`Win+Shift+S`）默认抓不到 layered window，
要用 `BitBlt + CAPTUREBLT` 才能截到。肉眼直接可见。

`-DemoIndicator` 模式下 indicator 用不透明红色 + 永久显示，方便远程
截屏验证位置/尺寸。

### 释放（Release）

越界到远端后，按 **A + S + D + F** 四键同时按下，鼠标键盘控制权回到 Windows。

---

## 文件布局

```
D:\tools\lan-mouse\
├── connect.ps1                       # 主脚本
├── config.toml                       # Windows daemon 配置（脚本自动生成）
├── .connect-config.json              # 用户偏好持久化
├── stdout.log / stderr.log           # lan-mouse Windows daemon 日志
├── clipsync-stdout/stderr.log        # clipsync Windows daemon 日志
├── USAGE.md                          # 本文档
├── bin\
│   ├── lan-mouse.exe                 # patched Windows binary
│   └── clipsync.exe                  # 剪贴板同步 daemon
└── patches\
    ├── uinput.rs                     # UOS 端 emulation backend
    ├── lib.rs                        # input-emulation/src/lib.rs
    ├── error.rs                      # input-emulation/src/error.rs
    ├── input-emulation-Cargo.toml    # 加 uinput feature
    ├── root-Cargo.toml               # 加 uinput_emulation feature
    └── windows-capture.rs            # Windows 端 sticky corners + V 形 indicator

D:\src\
├── lan-mouse-build\                  # patched lan-mouse 源码（target/ 缓存保留）
└── clipsync\                         # clipsync 源码
    ├── Cargo.toml
    └── src\main.rs
```

## 剪贴板同步 (clipsync)

跑 `connect.ps1` 时自动启动两端 clipsync daemon。**双向自动同步文本**：

- Windows 任意 app 按 **Ctrl+C** → ~500ms 内 UOS 剪贴板自动更新
- UOS 任意 GUI app 复制 → ~500ms 内 Windows 剪贴板自动更新
- 不会循环 echo（用 FNV hash 去重）
- 协议：`[u8 type][u32 BE length][N bytes payload]` TLV 帧
- 当前 `type=0x01` 仅支持文本；`0x02` PNG image / `0x03` files 已预留扩展点

**端口**：4243 (TCP)。`-ClipsyncPort N` 可改。`-NoClipsync` 禁用。

**协议扩展（图片/文件）**：改 4 个地方
1. `clipsync/src/main.rs::ClipboardItem` enum 加 variant
2. `type_byte()` / `payload()` / `from_frame()` 加分支
3. `read_local_clipboard()` / `write_local_clipboard()` 加平台实现
4. UOS 上 `cargo install --path . --force`，Windows 上 `cargo build --release` + 重新部署

**Linux 端实现**：write 同时调 `xclip`（X11 selection）+ `wl-copy`（Wayland clipboard），让 X
应用和 Wayland 应用都能粘贴；read 优先 `wl-paste`，fallback `xclip`。

### ⚠️ UOS Wayland 应用复制 → Windows 的限制

**症状**：UOS 上 Qt Wayland 应用（如默认启动的 deepin-editor）按 Ctrl+C 复制后，Windows 这边粘贴
不出来；但 Qt 应用之间（deepin-editor → deepin-terminal）能互相粘贴。

**原因**：

- dde-kwin 5.15 的 Wayland clipboard **没桥接到 X11 selection**（单向：Wayland → X 不同步）
- Wayland 协议要求读 clipboard 的 client 必须有 keyboard focus（安全限制：防止后台进程偷读）
- clipsync daemon 是 nohup 启动的，没 keyboard focus → `wl-paste` 拿不到 Qt Wayland 应用复制的内容
- dde-clipboard-daemon 不暴露 read 接口（只有写入 / Show GUI 这种 method）

**解决方案**：让 Qt 应用启动时用 X11 平台。

**方法 A（已对 deepin-editor 做）**：改 `.desktop` 文件让特定应用走 X11：

```bash
mkdir -p ~/.local/share/applications
cp /usr/share/applications/deepin-editor.desktop ~/.local/share/applications/
sed -i 's|^Exec=\(/usr/bin/\)\?deepin-editor|Exec=env QT_QPA_PLATFORM=xcb deepin-editor|g' \
    ~/.local/share/applications/deepin-editor.desktop
update-desktop-database ~/.local/share/applications
```

重启该应用后，复制走 X11 selection，clipsync 能读到，Windows 端能粘贴。

**方法 B（一次性影响所有 Qt 应用）**：在 `~/.profile` 末尾加：

```bash
export QT_QPA_PLATFORM=xcb
```

注销重新登录后所有 Qt 应用走 X11。**注意**：UOS 桌面 (DDE) 本身是 Qt 应用，全局强制 X11 可能影响
桌面外观（极少出现，但有风险）。建议优先方法 A。

**方法 C（接受现状）**：仅用 X11 应用复制（命令行 `xclip`、Firefox、Chrome 都默认 X11；只有
某些 Qt5/Qt6 应用走 Wayland）。

### 反向同步（Win → UOS）不受此限制

clipsync 写 UOS 时同时调 `xclip` + `wl-copy`，X 应用和 Wayland 应用都能粘贴。Win → UOS 全部
work，无需额外配置。

---

## 远端（Linux）现状

- `~/.cargo/bin/lan-mouse` — patched binary（uinput emulation backend）
- `~/.config/lan-mouse/config.toml` — 邻居配置
- `/etc/udev/rules.d/99-uinput-input-group.rules` — `/dev/uinput` 给 input 组 660
- `/etc/NetworkManager/conf.d/wifi-powersave-off.conf` — 关闭 WiFi 省电
- 用户在 `input` 组（uos 已加）

重启后 udev rule + NM config 自动生效，daemon 不会自启
（按需 `.\connect.ps1` 启动）。如需 daemon 自启，可在 UOS 加 systemd user unit
（待你需要时再做）。

---

## 故障排查

### `[1/4] 测试 SSH 连通性` 失败

脚本会区分**网络层**和**认证层**两类问题：

- **认证层（公钥免密没配）**：检测到 `Permission denied` 后会引导你**用密码登录一次**，
  自动把本机 `~/.ssh/id_ed25519.pub`（没有则现场生成）追加到远端 `~/.ssh/authorized_keys`，
  之后所有调用走免密。提示符如下：
  ```
  [!!] 远端未配置公钥免密登录
       现在用密码登录一次、把本机 SSH 公钥拷到远端？(Y/n):
  ```
  输 `Y` 回车 → 提示远端密码 → 输入 → 完成。下次跑就免密了。
- **网络层（连不上）**：Connection refused / timeout / Host unreachable 这类密码救不了，
  脚本直接报错退出。常见原因：
  1. **`-Target` 没带 `user@`**：例如 `-Target 10.81.194.113`。脚本会用 `-Hostname`（默认 `uos`）补成 `uos@10.81.194.113`；如果远端用户不叫 `uos`，必须显式写 `-Target <user>@<ip>`。
  2. **对方不在线 / IP 错 / 防火墙挡 22**：先 `ping <ip>` 确认主机活、`Test-NetConnection <ip> -Port 22` 确认 SSH 端口通。
  3. **known_hosts 冲突**：换了机器但 IP 复用，旧 host key 还在。修法：
     ```powershell
     ssh-keygen -R <ip>
     ```

手动验证 SSH 自己能不能登：
```powershell
ssh -o BatchMode=yes -o ConnectTimeout=5 uos@<ip> "echo ok"
```
能输出 `ok` 就说明免密通了，再回头跑 `connect.ps1`。

### `.\connect.ps1` 卡在 `[1/4] 测试 SSH 连通性` 不返回

通常是远端 ssh 公钥未配置 + 远端 prompt 密码导致 hang。Test-Ssh 内置 10s 硬超时，
正常会自动 `(ssh 连通性测试 10 秒未返回，已强制中断)`。如果连这个都不出，先：

```powershell
ssh-copy-id uos@192.168.137.27
```

### 鼠标越界后 UOS 鼠标卡在 (0,0) 不动

`/dev/uinput` 权限问题。检查：

```powershell
ssh uos@192.168.137.27 'stat -c %a:%U:%G /dev/uinput'
# 应该输出 660:root:input
```

如果不是 `660:root:input`，重跑 `.\connect.ps1 -Force` 会自动修。

### WiFi 频繁断流

NetworkManager / Win 移动热点的 WiFi 省电问题。`-Force` 重装会重设；
或手动：

```bash
ssh uos@192.168.137.27 'echo "1" | sudo -S nmcli connection modify <你的连接名> 802-11-wireless.powersave 2'
```

### 远端架构不是 amd64/arm64

脚本已自适应（rustup-init 自动选 host）。其他架构（如 mips/loong）不保证能装上 Rust。

---

## 重装/迁移

要在另一台 Windows 上从零部署：

1. 把 `D:\tools\lan-mouse\` 整个目录复制过去
2. 装 [Rust](https://rustup.rs/) + Visual Studio Build Tools（C++ workload）
3. 在新机器上 `cd D:\tools\lan-mouse-build` 后 `cargo build --release --no-default-features` 重编 Windows binary
4. 把 `target\release\lan-mouse.exe` 拷到 `bin\lan-mouse.exe`
5. `.\connect.ps1 -Setup` 跑配置向导
