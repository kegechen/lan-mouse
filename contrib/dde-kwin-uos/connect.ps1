<#
.SYNOPSIS
    Lan Mouse KVM 一键部署/连接（patched-uinput 版）。

.DESCRIPTION
    通过 SSH 探测远端环境，按需安装/启动 lan-mouse，并启动 Windows 端 daemon
    完成对接。第一次运行会进入交互配置向导（询问 SSH 目标 + UOS 方向 + 主机 IP）
    并保存到 .connect-config.json，后续运行无需再问。
    SSH 优先公钥免密；首次跑若未配免密会引导用密码登录一次、自动把本机公钥
    推到远端 authorized_keys，之后免密。安装时一次性 prompt sudo 密码。

.PARAMETER Target
    SSH 目标，格式 user@host（**必须带 user@**，否则会拿 Windows 用户名去登远端、SSH 直接失败）。
    传裸 IP 时脚本会用 -Hostname（默认 uos）补成 uos@host。覆盖配置文件。

.PARAMETER Direction
    UOS 在 Windows 桌面的方向：left/right/top/bottom。覆盖配置文件。

.PARAMETER Port
    lan-mouse UDP 端口。默认 4242。

.PARAMETER WinHostIp
    Windows 主机在远端能看到的 IP（通常 = ICS 网关 192.168.137.1，同 WiFi 场景填 Win 自己的 LAN IP）。
    覆盖配置文件。

.PARAMETER Hostname
    远端在 Windows 配置中的 hostname（仅 toml 显示用，同时也是 -Target 缺 user@ 时的兜底用户名）。默认 uos。

.PARAMETER DwellMs
    鼠标到边缘必须停留多少毫秒才越界（防误触）。0 = 碰边即过。

.PARAMETER ClipsyncPort
    剪贴板同步 TCP 端口。默认 4243。

.PARAMETER NoClipsync
    禁用剪贴板同步 daemon。

.PARAMETER DemoIndicator
    边缘 indicator 用不透明红色 + 永久显示，方便远程截屏调试。

.PARAMETER Setup
    强制进入交互配置向导（即使已有 .connect-config.json）。

.PARAMETER Force
    强制重新拉源码 + 重新编译远端 lan-mouse。

.PARAMETER Stop
    停止两端 daemon 后退出。

.EXAMPLE
    .\connect.ps1
    首次跑进交互向导，之后无参运行直接复用 .connect-config.json。

.EXAMPLE
    .\connect.ps1 -Target uos@192.168.137.27
    Windows 移动热点场景：UOS 通过 ICS 连过来，target 是固定的 192.168.137.x。

.EXAMPLE
    .\connect.ps1 -Target uos@10.81.194.113 -WinHostIp 10.81.194.50
    同 WiFi 场景：Win 和 UOS 在同一个路由器下，target/WinHostIp 都填实际 LAN IP。
    注意 -Target **必须带 uos@**。

.EXAMPLE
    .\connect.ps1 -Target 10.81.194.113
    传裸 IP 时脚本会自动补成 uos@10.81.194.113（用 -Hostname 字段，默认 uos）。

.EXAMPLE
    .\connect.ps1 -Direction right -DwellMs 200
    UOS 在 Windows 右边，鼠标顶住右边缘 200ms 才越界。

.EXAMPLE
    .\connect.ps1 -Stop
    停掉两端 daemon。

.EXAMPLE
    .\connect.ps1 -Force
    远端重拉源码重编（修复损坏安装、或拉到了新 patch 后强制更新）。
#>
[CmdletBinding()]
param(
    [string]$Target,
    [ValidateSet('left','right','top','bottom')]
    [string]$Direction,
    [int]$Port,
    [string]$WinHostIp,
    [string]$Hostname,
    [int]$DwellMs,
    [int]$ClipsyncPort = 4243,
    [switch]$NoClipsync,
    [switch]$DemoIndicator,
    [switch]$Setup,
    [switch]$Force,
    [switch]$Stop
)

$ErrorActionPreference = 'Stop'
$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$PatchDir    = Join-Path $ScriptDir 'patches'
$WinBinDir   = Join-Path $ScriptDir 'bin'
$WinExe      = Join-Path $WinBinDir 'lan-mouse.exe'
$WinCfg      = Join-Path $ScriptDir 'config.toml'
$ConfigPath  = Join-Path $ScriptDir '.connect-config.json'

# ---------- output helpers ----------
function Write-Step($msg) { Write-Host ("`n[==] " + $msg) -ForegroundColor Cyan }
function Write-OK($msg)   { Write-Host ("  [OK] " + $msg) -ForegroundColor Green }
function Write-Sub($msg)  { Write-Host ("       " + $msg) -ForegroundColor DarkGray }
function Write-Warn2($msg){ Write-Host ("  [!!] " + $msg) -ForegroundColor Yellow }
function Write-Err2($msg) { Write-Host ("  [XX] " + $msg) -ForegroundColor Red }

# ---------- config load / save ----------
function Load-Config {
    if (Test-Path $ConfigPath) {
        try { return (Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json) }
        catch { Write-Warn2 "配置文件损坏，将忽略: $ConfigPath"; return $null }
    }
    return $null
}
function Save-Config($cfg) {
    $cfg | ConvertTo-Json | Set-Content -Path $ConfigPath -Encoding UTF8
}

# ---------- interactive wizard ----------
function Run-SetupWizard {
    param($Existing)
    Write-Host ""
    Write-Host "  === 配置向导（首次运行 / -Setup） ===" -ForegroundColor Cyan
    Write-Host ""

    # 1. SSH target
    $defT = if ($Existing -and $Existing.Target) { $Existing.Target } else { 'uos@192.168.137.27' }
    Write-Host "  1. SSH 目标地址" -ForegroundColor White
    Write-Host "     格式: user@host  (远端 Linux 机器，需已配好公钥免密 ssh)"
    $t = Read-Host "     [回车 = $defT]"
    if ([string]::IsNullOrWhiteSpace($t)) { $t = $defT }

    # 2. Direction
    $defDir = if ($Existing -and $Existing.Direction) { $Existing.Direction } else { 'left' }
    $defNum = @{ left=1; right=2; top=3; bottom=4 }[$defDir]
    Write-Host ""
    Write-Host "  2. 远端机器在 Windows 桌面的物理位置" -ForegroundColor White
    Write-Host "     (鼠标会从这个方向滑出 Windows 屏幕越界过去)"
    Write-Host "       1) [<-] 左侧 (left)"
    Write-Host "       2) [->] 右侧 (right)"
    Write-Host "       3) [^]  上方 (top)"
    Write-Host "       4) [v]  下方 (bottom)"
    $dnum = Read-Host "     [回车 = $defNum]"
    if ([string]::IsNullOrWhiteSpace($dnum)) { $dnum = $defNum }
    if ($dnum -notmatch '^[1-4]$') {
        Write-Warn2 "无效输入，使用默认 $defDir"
        $dnum = $defNum
    }
    $dir = @('left','right','top','bottom')[[int]$dnum - 1]

    # 3. Windows host IP (visible from remote)
    $defIp = if ($Existing -and $Existing.WinHostIp) { $Existing.WinHostIp } else { '192.168.137.1' }
    Write-Host ""
    Write-Host "  3. Windows 主机在远端能访问到的 IP" -ForegroundColor White
    Write-Host "     (Win10/11 移动热点共享时通常是 192.168.137.1，即 ICS 网关)"
    $ip = Read-Host "     [回车 = $defIp]"
    if ([string]::IsNullOrWhiteSpace($ip)) { $ip = $defIp }

    # 4. Port (advanced; usually default)
    $defP = if ($Existing -and $Existing.Port) { $Existing.Port } else { 4242 }
    Write-Host ""
    Write-Host "  4. lan-mouse UDP 端口" -ForegroundColor White
    $pIn = Read-Host "     [回车 = $defP]"
    if ([string]::IsNullOrWhiteSpace($pIn)) { $pIn = $defP }
    if ($pIn -notmatch '^\d+$') {
        Write-Warn2 "无效端口，使用默认 $defP"
        $pIn = $defP
    }

    $newCfg = [PSCustomObject]@{
        Target    = $t
        Direction = $dir
        WinHostIp = $ip
        Port      = [int]$pIn
        Hostname  = if ($Existing -and $Existing.Hostname) { $Existing.Hostname } else { 'uos' }
    }
    Save-Config $newCfg
    Write-Host ""
    Write-OK "配置已保存: $ConfigPath"
    Write-Sub ("   target=" + $newCfg.Target + ", direction=" + $newCfg.Direction + ", win-ip=" + $newCfg.WinHostIp + ", port=" + $newCfg.Port)
    return $newCfg
}

# ---------- ssh / scp helpers ----------
# -o LogLevel=ERROR 抑制 "WARNING: connection is not using a post-quantum..."
# 那行 stderr 警告——它会让 PS 5.1 在 2>&1 合并流时卡住等不到 EOF。
$script:SshOpts = @('-o','LogLevel=ERROR','-o','ServerAliveInterval=5','-o','ServerAliveCountMax=3')

function Invoke-Ssh {
    param([string]$Cmd, [string]$Stdin = $null)
    if ($null -ne $Stdin) {
        # PS 5.1 的 "$str | & ssh.exe" 不会正确关闭 ssh 的 stdin，
        # 导致远端 `bash -s` 死等 EOF。改用 ProcessStartInfo 显式 Close。
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = 'ssh.exe'
        $argList = @($script:SshOpts) + @($script:ResolvedTarget, $Cmd)
        $psi.Arguments = ($argList | ForEach-Object {
            if ($_ -match '[\s"]') { '"' + ($_ -replace '"','\"') + '"' } else { $_ }
        }) -join ' '
        $psi.UseShellExecute        = $false
        $psi.RedirectStandardInput  = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError  = $true
        $psi.CreateNoWindow         = $true
        $proc = [System.Diagnostics.Process]::Start($psi)
        $proc.StandardInput.Write($Stdin)
        $proc.StandardInput.Close()
        if (-not $proc.WaitForExit(60000)) {
            try { $proc.Kill() } catch {}
            return "(timeout 60s)"
        }
        $out = $proc.StandardOutput.ReadToEnd() + $proc.StandardError.ReadToEnd()
        return $out
    }
    return (& ssh.exe @script:SshOpts $script:ResolvedTarget $Cmd 2>&1)
}
function Test-Ssh {
    # 返回 hashtable：ok=$bool，reason='auth|network|timeout'，output=ssh 原始输出。
    # 用 Start-Job 包硬超时，避免 ssh.exe hang 时整个脚本卡住。
    $job = Start-Job -ScriptBlock {
        param($t)
        & ssh.exe -o ConnectTimeout=5 `
                  -o BatchMode=yes `
                  -o StrictHostKeyChecking=accept-new `
                  -o PasswordAuthentication=no `
                  -o KbdInteractiveAuthentication=no `
                  $t "echo ok" 2>&1
    } -ArgumentList $script:ResolvedTarget
    $waited = Wait-Job -Job $job -Timeout 10
    if ($null -eq $waited) {
        try { Stop-Job $job -ErrorAction SilentlyContinue } catch {}
        try { Remove-Job $job -Force -ErrorAction SilentlyContinue } catch {}
        Write-Sub "(ssh 连通性测试 10 秒未返回，已强制中断)"
        return @{ ok=$false; reason='timeout'; output='' }
    }
    $r = Receive-Job $job 2>&1
    Remove-Job $job -Force -ErrorAction SilentlyContinue
    $text = ($r | Out-String)
    # 必须是独占一行的 "ok"，否则远端 MOTD/banner 含 "ok" 字样会误判通过。
    if ($text -match '(?m)^ok\s*$') { return @{ ok=$true; reason=''; output=$text } }
    # 公钥被拒（或 BatchMode 拒绝交互认证）=> auth 问题，可用密码补救。
    # Connection refused / timeout / Host unreachable 是网络层问题、密码救不了。
    if ($text -match 'Permission denied|publickey|keyboard-interactive|password') {
        return @{ ok=$false; reason='auth'; output=$text }
    }
    return @{ ok=$false; reason='network'; output=$text }
}

function Enable-KeyAuth {
    # 用户没配公钥免密时，提示用密码登录一次、把本机公钥追加到远端 authorized_keys。
    # 成功返回 $true（之后所有 ssh/scp 调用自动走免密），失败返回 $false。
    Write-Warn2 "远端未配置公钥免密登录"
    $choice = Read-Host "    现在用密码登录一次、把本机 SSH 公钥拷到远端？(Y/n)"
    if ($choice -match '^[nN]') {
        Write-Sub ("已取消。手动配置：ssh-copy-id " + $script:ResolvedTarget)
        return $false
    }

    # 1. 本机准备公钥（优先 ed25519，无则 RSA，都没有就生成 ed25519）
    $sshDir = Join-Path $env:USERPROFILE '.ssh'
    if (-not (Test-Path $sshDir)) {
        New-Item -ItemType Directory -Path $sshDir -Force | Out-Null
    }
    $edPub  = Join-Path $sshDir 'id_ed25519.pub'
    $rsaPub = Join-Path $sshDir 'id_rsa.pub'
    $pubFile = $null
    if     (Test-Path $edPub)  { $pubFile = $edPub  }
    elseif (Test-Path $rsaPub) { $pubFile = $rsaPub }
    else {
        Write-Sub "本机无 SSH 密钥，生成 ed25519 (无 passphrase)..."
        $edKey = Join-Path $sshDir 'id_ed25519'
        # PS 5.1 native-call 对 `-N ""` 传参不稳；用 ProcessStartInfo 显式控制 args+stdin。
        # ssh-keygen 在 -f 已指定但缺 -N 时会从 stdin 读两次 passphrase（输入+确认），喂两个空行。
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = 'ssh-keygen.exe'
        $psi.Arguments = "-t ed25519 -q -f `"$edKey`""
        $psi.UseShellExecute        = $false
        $psi.RedirectStandardInput  = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError  = $true
        $psi.CreateNoWindow         = $true
        $kp = [System.Diagnostics.Process]::Start($psi)
        $kp.StandardInput.WriteLine('')
        $kp.StandardInput.WriteLine('')
        $kp.StandardInput.Close()
        if (-not $kp.WaitForExit(10000)) { try { $kp.Kill() } catch {} }
        if ($kp.ExitCode -ne 0 -or -not (Test-Path $edPub)) {
            Write-Err2 "ssh-keygen 失败，无法生成密钥"
            return $false
        }
        $pubFile = $edPub
    }
    Write-Sub ("使用公钥: " + $pubFile)

    # 2. 用密码登录、把 pubkey 追加进 authorized_keys。
    #    BatchMode=no + PreferredAuthentications=password,keyboard-interactive 让 ssh 直接 prompt
    #    密码到当前终端（Invoke-Ssh 不能用 —— 它 BatchMode 写死、且会重定向 stdin/stderr）。
    #    PubkeyAuthentication=no 跳过失败的 pubkey 尝试，直接进密码流程。
    $pubContent = (Get-Content $pubFile -Raw).Trim()
    if ([string]::IsNullOrWhiteSpace($pubContent)) {
        Write-Err2 ("公钥文件为空: " + $pubFile)
        return $false
    }
    # 单引号包 pubkey 防 bash 二次展开（ssh 公钥本身不含单引号，安全）
    $remoteCmd = "umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; grep -qxF '$pubContent' ~/.ssh/authorized_keys || echo '$pubContent' >> ~/.ssh/authorized_keys"
    Write-Sub "下面会提示远端密码（只需输入一次）"
    & ssh.exe -o BatchMode=no `
              -o StrictHostKeyChecking=accept-new `
              -o PreferredAuthentications=password,keyboard-interactive `
              -o PubkeyAuthentication=no `
              $script:ResolvedTarget $remoteCmd
    if ($LASTEXITCODE -ne 0) {
        Write-Err2 "密码登录或公钥写入失败"
        Write-Sub "（远端 sshd 可能也禁用了密码登录 PasswordAuthentication=no，需管理员手动添加公钥）"
        return $false
    }
    Write-OK "公钥已写入远端 ~/.ssh/authorized_keys"

    # 3. 复测免密
    $retest = Test-Ssh
    if (-not $retest.ok) {
        Write-Err2 "公钥已传，但免密复测仍失败:"
        Write-Sub $retest.output
        Write-Sub "（如果本机私钥需 passphrase，先运行 ssh-add 把私钥加进 ssh-agent）"
        return $false
    }
    return $true
}
function Copy-ToRemote {
    param([string]$Local, [string]$Remote)
    # 源文件不存在 / scp 非零退出 都要立刻 fail —— 之前静默 Out-Null 把 exit 255 吞掉，
    # 让远端 install-user.sh 用一个不存在的 /tmp/xxx 文件，set -eu 才在那里 abort，根因被掩盖
    if (-not (Test-Path -LiteralPath $Local)) {
        Write-Err2 "上传源文件不存在: $Local"
        exit 1
    }
    & scp.exe -q $Local "$($script:ResolvedTarget):$Remote" 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Err2 "scp 上传失败（exit $LASTEXITCODE）: $Local -> $Remote"
        exit 1
    }
}

# ---------- probe remote state ----------
function Probe-Remote {
    $script = @'
INSTALLED=$([ -x "$HOME/.cargo/bin/lan-mouse" ] && echo yes || echo no)
RUNNING=$(pgrep -f "cargo/bin/lan-mouse -d" >/dev/null 2>&1 && echo yes || echo no)
UINPUT_PERMS=$(stat -c "%a:%U:%G" /dev/uinput 2>/dev/null)
UINPUT_OK=$([ -w /dev/uinput ] && echo yes || echo no)
ARCH=$(uname -m)
ACTIVE_CONN=$(nmcli -t -f NAME c show --active 2>/dev/null | head -1)
NM_PS="?"
if [ -n "$ACTIVE_CONN" ]; then
    NM_PS=$(nmcli -t -f 802-11-wireless.powersave c show "$ACTIVE_CONN" 2>/dev/null | awk -F: '{print $2}')
fi
LAN_VER=""
[ -x "$HOME/.cargo/bin/lan-mouse" ] && LAN_VER=$($HOME/.cargo/bin/lan-mouse --version 2>&1 | head -1)
# UOS 看到的 SSH 源 IP = Windows 在当前路径下给 UOS 发包的真实源 IP，
# 写入 UOS toml 的 [neighbor].ips 才能让 UOS 收到 ping/input 事件。
WIN_IP="${SSH_CLIENT%% *}"
echo "INSTALLED=$INSTALLED"
echo "RUNNING=$RUNNING"
echo "UINPUT_PERMS=$UINPUT_PERMS"
echo "UINPUT_OK=$UINPUT_OK"
echo "ARCH=$ARCH"
echo "NM_POWERSAVE=$NM_PS"
echo "LAN_VER=$LAN_VER"
echo "WIN_IP=$WIN_IP"
'@
    $out = Invoke-Ssh "bash -s" -Stdin $script
    $h = @{}
    foreach ($line in ($out -split "`r?`n")) {
        if ($line -match '^([A-Z_]+)=(.*)$') { $h[$matches[1]] = $matches[2].Trim() }
    }
    return $h
}

# ---------- install on remote ----------
function Install-Remote {
    param([string]$SudoPass)

    $opp = @{ left='right'; right='left'; top='bottom'; bottom='top' }[$script:ResolvedDirection]

    Write-Sub "上传 5 个 patch 文件..."
    foreach ($pair in @(
        @('uinput.rs',                  '/tmp/lan-mouse-patches-uinput.rs'),
        @('lib.rs',                     '/tmp/lan-mouse-patches-lib.rs'),
        @('error.rs',                   '/tmp/lan-mouse-patches-error.rs'),
        @('input-emulation-Cargo.toml', '/tmp/lan-mouse-patches-ie-Cargo.toml'),
        @('root-Cargo.toml',            '/tmp/lan-mouse-patches-root-Cargo.toml'))) {
        $local  = Join-Path $PatchDir $pair[0]
        $remote = $pair[1]
        Copy-ToRemote -Local $local -Remote $remote
    }

    Write-Sub "上传 UOS config.toml..."
    $uosCfg = @"
# generated by connect.ps1
release_bind = ["KeyRightalt"]
port = $($script:ResolvedPort)

[$opp]
hostname = "win-host"
ips = ["$($script:ResolvedWinHostIp)"]
port = $($script:ResolvedPort)
"@
    $tmpCfg = Join-Path $env:TEMP 'lan-mouse-uos-config.toml'
    [System.IO.File]::WriteAllText($tmpCfg, $uosCfg, [System.Text.UTF8Encoding]::new($false))
    Copy-ToRemote -Local $tmpCfg -Remote '/tmp/lan-mouse-config.toml'

    Write-Sub "上传 install.sh / install-user.sh..."
    $rootSh = @'
#!/bin/bash
set -eu
TARGET_USER="${SUDO_USER:-$(logname 2>/dev/null || echo unknown)}"
USER_HOME=$(getent passwd "$TARGET_USER" | cut -d: -f6)
[ -n "$USER_HOME" ] || { echo "cannot resolve home dir for $TARGET_USER"; exit 1; }

echo "[1/8] fix WiFi powersave + DNS"
ACTIVE_DEV=$(nmcli -t -f DEVICE,STATE d 2>/dev/null | awk -F: '$2=="connected"{print $1; exit}')
ACTIVE_CONN=$(nmcli -t -f NAME c show --active 2>/dev/null | head -1)
if [ -n "$ACTIVE_CONN" ]; then
    nmcli connection modify "$ACTIVE_CONN" 802-11-wireless.powersave 2 2>/dev/null || true
    nmcli connection modify "$ACTIVE_CONN" 802-11-wireless.wake-on-wlan ignore 2>/dev/null || true
    # 仅在当前 DNS 解析不了下载源时才覆盖——企业内网常配私有 DNS 解内部域名，
    # 直接强写公网 DNS 会破坏 OA/Wiki/内部 Git 解析
    if ! getent hosts rsproxy.cn >/dev/null 2>&1 && ! getent hosts github.com >/dev/null 2>&1; then
        echo "  (当前 DNS 解析不了 rsproxy.cn/github.com，临时改公网 DNS)"
        nmcli connection modify "$ACTIVE_CONN" ipv4.dns "119.29.29.29 223.5.5.5 8.8.8.8" 2>/dev/null || true
        nmcli connection modify "$ACTIVE_CONN" ipv4.ignore-auto-dns yes 2>/dev/null || true
    else
        echo "  (DNS 解析正常，不动用户 DNS 配置)"
    fi
    [ -n "$ACTIVE_DEV" ] && nmcli device reapply "$ACTIVE_DEV" 2>/dev/null || true
fi
mkdir -p /etc/NetworkManager/conf.d
printf '[connection]\nwifi.powersave = 2\n' > /etc/NetworkManager/conf.d/wifi-powersave-off.conf

echo "[2/8] /dev/uinput perms (input group, mode 660)"
cat > /etc/udev/rules.d/99-uinput-input-group.rules <<'RULE'
KERNEL=="uinput", GROUP="input", MODE="0660"
RULE
udevadm control --reload-rules 2>/dev/null || true
chgrp input /dev/uinput 2>/dev/null || true
chmod 660 /dev/uinput 2>/dev/null || true
id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx input || usermod -aG input "$TARGET_USER"

echo "[3/8] apt deps (libx11-dev libxtst-dev — may fail on UOS due to system pkg conflicts, ignore)"
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libx11-dev libxtst-dev 2>&1 | tail -3 || true

echo "[4-8/8] handing off to user-side install"
chown "$TARGET_USER":"$TARGET_USER" /tmp/install-user.sh
chmod +x /tmp/install-user.sh
sudo -u "$TARGET_USER" -H bash /tmp/install-user.sh
'@
    $userSh = @'
#!/bin/bash
set -eu
# WIN_IP 由 connect.ps1 在生成本脚本时填入（来自配置或 UOS 自动探测到的 SSH 源 IP）
WIN_IP="__WIN_IP__"
PROXY_CANDIDATE="http://${WIN_IP}:10808"

# 优先走 Win 端代理（ICS/GFW 场景常见），探测不通则走直连——UOS 在普通 LAN 下自己能上网
if curl -fsS --connect-timeout 3 --proxy "$PROXY_CANDIDATE" -o /dev/null https://sh.rustup.rs 2>/dev/null; then
    PROXY="$PROXY_CANDIDATE"
    export HTTP_PROXY="$PROXY" HTTPS_PROXY="$PROXY" http_proxy="$PROXY" https_proxy="$PROXY"
    echo "(走代理 $PROXY)"
else
    PROXY=""
    echo "(Win 代理 $PROXY_CANDIDATE 不可达，走直连)"
fi

echo "[4/8] Rust toolchain"
if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
    chmod +x /tmp/rustup-init.sh
    RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
        bash /tmp/rustup-init.sh -y --default-toolchain stable --profile minimal --no-modify-path
fi
. "$HOME/.cargo/env"

echo "[5/8] cargo proxy + crates.io mirror"
mkdir -p "$HOME/.cargo"
{
    if [ -n "$PROXY" ]; then
        echo "[http]"
        echo "proxy = \"$PROXY\""
        echo "[https]"
        echo "proxy = \"$PROXY\""
    fi
    cat <<EOF
[net]
git-fetch-with-cli = true
[source.crates-io]
replace-with = "rsproxy-sparse"
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
EOF
} > "$HOME/.cargo/config.toml"

echo "[6/8] clone lan-mouse v0.10.0 + apply patches"
mkdir -p "$HOME/src"
cd "$HOME/src"
[ -d lan-mouse ] || git clone --depth 1 --branch v0.10.0 https://github.com/feschber/lan-mouse.git
cp /tmp/lan-mouse-patches-uinput.rs        lan-mouse/input-emulation/src/uinput.rs
cp /tmp/lan-mouse-patches-lib.rs           lan-mouse/input-emulation/src/lib.rs
cp /tmp/lan-mouse-patches-error.rs         lan-mouse/input-emulation/src/error.rs
cp /tmp/lan-mouse-patches-ie-Cargo.toml    lan-mouse/input-emulation/Cargo.toml
cp /tmp/lan-mouse-patches-root-Cargo.toml  lan-mouse/Cargo.toml

echo "[7/8] cargo install (1-3 minutes on first build)"
cd lan-mouse
cargo install --locked --path . --no-default-features --features uinput_emulation --force

echo "[8/8] deploy uos config"
mkdir -p "$HOME/.config/lan-mouse"
cp /tmp/lan-mouse-config.toml "$HOME/.config/lan-mouse/config.toml"

echo "BUILD_DONE"
ls -la "$HOME/.cargo/bin/lan-mouse"
'@
    # 把当前 Win 主机 IP 注入 install-user.sh 的代理探测逻辑（用 ResolvedWinHostIp，
    # 它在 Probe-Remote 后已经被 SSH 源 IP 自动校正）
    $userSh = $userSh.Replace('__WIN_IP__', $script:ResolvedWinHostIp)
    $tmpRoot = Join-Path $env:TEMP 'lan-mouse-install.sh'
    $tmpUser = Join-Path $env:TEMP 'lan-mouse-install-user.sh'
    [System.IO.File]::WriteAllText($tmpRoot, $rootSh, [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText($tmpUser, $userSh, [System.Text.UTF8Encoding]::new($false))
    Copy-ToRemote -Local $tmpRoot -Remote '/tmp/install.sh'
    Copy-ToRemote -Local $tmpUser -Remote '/tmp/install-user.sh'

    Write-Sub "转 LF + 设置可执行权限"
    Invoke-Ssh "sed -i 's/\r$//' /tmp/install.sh /tmp/install-user.sh; chmod +x /tmp/install.sh /tmp/install-user.sh" | Out-Null

    Write-Step "执行远端安装（约 2-4 分钟）"
    $combined = $SudoPass + "`n"
    $combined | & ssh.exe $script:ResolvedTarget "sudo -S -p '' bash /tmp/install.sh" 2>&1 | ForEach-Object {
        if     ($_ -match 'BUILD_DONE')             { Write-OK "编译完成，binary 已就位" }
        elseif ($_ -match '^\[\d/8\]')              { Write-Sub $_ }
        elseif ($_ -match 'Compiling lan-mouse ')   { Write-Sub "    -> 链接最终 binary..." }
        elseif ($_ -match 'error\[E|error: failed') { Write-Err2 $_ }
        elseif ($_ -match '验证成功')                { } # silence sudo cn-locale prompt confirmation
        else { Write-Sub $_ }
    }
    if ($LASTEXITCODE -ne 0) {
        Write-Err2 "远端安装失败（exit $LASTEXITCODE）"
        exit 1
    }
}

# ---------- start remote daemon ----------
function Start-RemoteClipsync {
    if ($NoClipsync) { return }
    Write-Step "启动远端 clipsync (listen :$($script:ResolvedClipsyncPort))"
    $port = $script:ResolvedClipsyncPort
    # 用 here-string + bash -s 方式喂脚本，避免 ssh 单行命令对 ; & 的 quote bug
    # WAYLAND_DISPLAY 必须显式 export — ssh non-interactive shell 不继承 wayland session
    $script = @"
pkill -f 'cargo/bin/clipsync' 2>/dev/null || true
sleep 1
rm -f /tmp/clipsync.log
if [ ! -x "`$HOME/.cargo/bin/clipsync" ]; then
    echo NOT_INSTALLED
    exit 0
fi
export WAYLAND_DISPLAY=wayland-0
export XDG_RUNTIME_DIR=/run/user/1000
export DISPLAY=:0
export XAUTHORITY=`$HOME/.Xauthority
nohup "`$HOME/.cargo/bin/clipsync" --listen 0.0.0.0:$port > /tmp/clipsync.log 2>&1 < /dev/null &
disown
sleep 1
pgrep -f cargo/bin/clipsync >/dev/null && echo OK || echo FAIL
"@
    $out = Invoke-Ssh "bash -s" -Stdin $script
    $joined = ($out -join "`n")
    if ($joined -match 'NOT_INSTALLED') {
        Write-Warn2 "远端无 clipsync — 跑 .\connect.ps1 -Force 让远端自动安装"
    } elseif ($joined -match 'OK') {
        Write-OK "远端 clipsync 已 listen :$port"
    } else {
        Write-Warn2 "远端 clipsync 状态不明: $joined"
    }
}

function Ensure-RemoteConfig {
    # 用当前 ResolvedWinHostIp/Direction/Port 生成 UOS toml，写到远端
    # ~/.config/lan-mouse/config.toml，内容若变化返回 $true（调用方据此重启 daemon）。
    $opp = @{ left='right'; right='left'; top='bottom'; bottom='top' }[$script:ResolvedDirection]
    $uosCfg = @"
# generated by connect.ps1
release_bind = ["KeyRightalt"]
port = $($script:ResolvedPort)

[$opp]
hostname = "win-host"
ips = ["$($script:ResolvedWinHostIp)"]
port = $($script:ResolvedPort)
"@
    # Linux 文件统一 LF：避免 here-string 的 CRLF 跟 UOS 端原 LF 文件 cmp 永远不等，
    # 每次跑都"误判变化"重启 daemon。
    $uosCfg = $uosCfg -replace "`r`n", "`n"
    $tmpCfg = Join-Path $env:TEMP 'lan-mouse-uos-config.toml'
    [System.IO.File]::WriteAllText($tmpCfg, $uosCfg, [System.Text.UTF8Encoding]::new($false))
    Copy-ToRemote -Local $tmpCfg -Remote '/tmp/lan-mouse-config.toml'
    $cmp = Invoke-Ssh "bash -s" -Stdin @'
mkdir -p "$HOME/.config/lan-mouse"
TARGET="$HOME/.config/lan-mouse/config.toml"
if [ -f "$TARGET" ] && cmp -s "$TARGET" /tmp/lan-mouse-config.toml; then
    echo UNCHANGED
else
    cp /tmp/lan-mouse-config.toml "$TARGET"
    echo CHANGED
fi
'@
    return ((($cmp -join "`n")) -match 'CHANGED')
}
function Start-RemoteDaemon {
    Write-Step "启动远端 lan-mouse daemon"
    $cmd = @'
pkill -f "cargo/bin/lan-mouse -d" 2>/dev/null || true
sleep 1
rm -f /tmp/lan-mouse.log
nohup "$HOME/.cargo/bin/lan-mouse" -d > /tmp/lan-mouse.log 2>&1 < /dev/null &
disown
sleep 2
tail -10 /tmp/lan-mouse.log
'@
    $out = Invoke-Ssh "bash -s" -Stdin $cmd
    $joined = ($out -join "`n")
    if ($joined -match 'using emulation backend: uinput') {
        Write-OK "uinput backend 启动"
    } elseif ($joined -match 'using emulation backend:') {
        Write-Warn2 "启动了，但未使用 uinput backend"
        Write-Sub $joined
    } else {
        Write-Warn2 "daemon 状态待确认，日志:"
        Write-Sub $joined
    }
}
function Activate-Remote {
    Write-Step "激活远端 client 0"
    # 用 stdin 喂 cli 命令；远端 timeout 4s 防卡死；ssh 自身 8s connect timeout
    $script = @"
printf "activate 0\nlist\n" | timeout 4 "`$HOME/.cargo/bin/lan-mouse" -f cli 2>&1
"@
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = 'ssh.exe'
    $argList = @('-o','ConnectTimeout=8','-o','LogLevel=ERROR','-o','ServerAliveInterval=3','-o','ServerAliveCountMax=2',$script:ResolvedTarget,'bash -s')
    $psi.Arguments = ($argList | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"','\"') + '"' } else { $_ }
    }) -join ' '
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    $proc.StandardInput.Write($script)
    $proc.StandardInput.Close()
    if (-not $proc.WaitForExit(15000)) {
        try { $proc.Kill() } catch {}
        Write-Warn2 "激活超时（15s），跳过 — 远端 daemon 或 SSH 不响应"
        return
    }
    $out = $proc.StandardOutput.ReadToEnd() + $proc.StandardError.ReadToEnd()
    if ($out -match 'active: true') {
        Write-OK "远端 client 0 已激活"
    } else {
        Write-Warn2 "激活状态待确认: $out"
    }
}

# ---------- local (Windows) daemon ----------
function Ensure-LocalConfig {
    $localCfg = @"
# generated by connect.ps1
release_bind = ["KeyRightalt"]
port = $($script:ResolvedPort)

[$($script:ResolvedDirection)]
hostname = "$($script:ResolvedHostname)"
ips = ["$($script:ResolvedTarget -replace '^.*@','' -replace ':.*$','')"]
port = $($script:ResolvedPort)
"@
    [System.IO.File]::WriteAllText($WinCfg, $localCfg, [System.Text.UTF8Encoding]::new($false))
    Write-Sub "Windows config: $WinCfg"
}
function Start-LocalDaemon {
    if (-not (Test-Path $WinExe)) {
        Write-Err2 "找不到 Windows lan-mouse: $WinExe"
        Write-Sub "下载并解压 lan-mouse-windows.zip 到 $WinBinDir"
        exit 1
    }
    Ensure-LocalConfig
    Write-Step "启动 Windows daemon"
    Get-Process lan-mouse -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    $stdoutLog = Join-Path $ScriptDir 'stdout.log'
    $stderrLog = Join-Path $ScriptDir 'stderr.log'
    Remove-Item $stdoutLog,$stderrLog -ErrorAction SilentlyContinue
    # patched binary 识别此环境变量：鼠标必须在屏幕边界停留 N 毫秒才越界（防误触）
    $env:LAN_MOUSE_DWELL_MS = "$($script:ResolvedDwellMs)"
    # 让 lan-mouse 把 input_capture 的 dwell trace 写到 stderr.log（方便远程诊断）
    $env:RUST_LOG = "info,input_capture=debug"
    # demo 模式：indicator 启动后一直显示在屏幕左中，进度条循环动画，远程截图查看
    if ($DemoIndicator) { $env:LAN_MOUSE_INDICATOR_DEMO = "1" } else { Remove-Item Env:LAN_MOUSE_INDICATOR_DEMO -ErrorAction SilentlyContinue }
    Write-Sub ("dwell time = " + $script:ResolvedDwellMs + " ms  (RUST_LOG=info,input_capture=debug)")
    if ($DemoIndicator) { Write-Sub "INDICATOR DEMO MODE: 永久显示在屏幕左中，进度条 1.5s 循环" }
    Start-Process -FilePath $WinExe `
                  -ArgumentList @('-d','-c',$WinCfg) `
                  -WindowStyle Hidden `
                  -RedirectStandardOutput $stdoutLog `
                  -RedirectStandardError  $stderrLog | Out-Null
    Start-Sleep -Seconds 2
    if (Get-Process lan-mouse -ErrorAction SilentlyContinue) {
        Write-OK "Windows daemon 启动"
    } else {
        Write-Err2 "Windows daemon 启动失败，看 $stderrLog"
        exit 1
    }
}
function Activate-Local {
    Write-Step "激活 Windows client 0"
    "activate 0`nlist" | & $WinExe -f cli -c $WinCfg 2>&1 | Out-Null
    Write-OK "Windows client 0 已激活"
}

function Start-LocalClipsync {
    if ($NoClipsync) { return }
    $exe = Join-Path $WinBinDir 'clipsync.exe'
    if (-not (Test-Path $exe)) {
        Write-Warn2 "找不到 clipsync.exe — 已跳过"
        return
    }
    Write-Step "启动 Windows clipsync (connect $($script:ResolvedClipsyncTarget):$($script:ResolvedClipsyncPort))"
    Get-Process clipsync -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    $stdoutLog = Join-Path $ScriptDir 'clipsync-stdout.log'
    $stderrLog = Join-Path $ScriptDir 'clipsync-stderr.log'
    Remove-Item $stdoutLog,$stderrLog -ErrorAction SilentlyContinue
    Start-Process -FilePath $exe `
                  -ArgumentList @(
                      '--connect', "$($script:ResolvedClipsyncTarget):$($script:ResolvedClipsyncPort)"
                  ) `
                  -WindowStyle Hidden `
                  -RedirectStandardOutput $stdoutLog `
                  -RedirectStandardError  $stderrLog | Out-Null
    Start-Sleep -Milliseconds 500
    if (Get-Process clipsync -ErrorAction SilentlyContinue) {
        Write-OK "Windows clipsync 启动 (双向自动同步剪贴板文本)"
    } else {
        Write-Warn2 "Windows clipsync 启动失败，看 $stderrLog"
    }
}

# ---------- stop ----------
function Stop-AllDaemons {
    Write-Step "停止两端 daemon (lan-mouse + clipsync)"
    Invoke-Ssh "pkill -f 'cargo/bin/lan-mouse -d' 2>/dev/null; pkill -f 'cargo/bin/clipsync' 2>/dev/null; true" | Out-Null
    Get-Process lan-mouse,clipsync -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Write-OK "已停止"
}

# ============================== MAIN ==============================
$existing = Load-Config

# 决定是否进向导：显式 -Setup 或 没 config 文件且没传参覆盖
$noConfig = ($null -eq $existing)
$hasOverride = $PSBoundParameters.ContainsKey('Target') -or $PSBoundParameters.ContainsKey('Direction') -or $PSBoundParameters.ContainsKey('WinHostIp')
$enterWizard = $Setup -or ($noConfig -and -not $hasOverride)

if ($enterWizard) {
    $cfg = Run-SetupWizard $existing
} else {
    $cfg = if ($existing) { $existing } else {
        # 用户传了部分参数但没 config，构造最小 cfg + 用默认值补全
        [PSCustomObject]@{
            Target    = if ($Target)    { $Target }    else { 'uos@192.168.137.27' }
            Direction = if ($Direction) { $Direction } else { 'left' }
            WinHostIp = if ($WinHostIp) { $WinHostIp } else { '192.168.137.1' }
            Port      = if ($Port)      { $Port }      else { 4242 }
            Hostname  = if ($Hostname)  { $Hostname }  else { 'uos' }
        }
    }
}

# CLI 参数覆盖 cfg
if ($PSBoundParameters.ContainsKey('Target'))    { $cfg.Target = $Target }
if ($PSBoundParameters.ContainsKey('Direction')) { $cfg.Direction = $Direction }
if ($PSBoundParameters.ContainsKey('WinHostIp')) { $cfg.WinHostIp = $WinHostIp }
if ($PSBoundParameters.ContainsKey('Port'))      { $cfg.Port = $Port }
if ($PSBoundParameters.ContainsKey('Hostname'))  { $cfg.Hostname = $Hostname }
if ($PSBoundParameters.ContainsKey('DwellMs'))   {
    if ($cfg.PSObject.Properties.Name -contains 'DwellMs') { $cfg.DwellMs = $DwellMs }
    else { $cfg | Add-Member -NotePropertyName DwellMs -NotePropertyValue $DwellMs -Force }
}

# 落到 script-scope 变量供函数引用
$script:ResolvedTarget    = $cfg.Target
$script:ResolvedDirection = $cfg.Direction
$script:ResolvedWinHostIp = $cfg.WinHostIp
$script:ResolvedPort      = [int]$cfg.Port
$script:ResolvedHostname  = $cfg.Hostname

# 容错：-Target 传的是裸 IP（不含 user@）时，用 Hostname 补 user@，否则 ssh 会拿 Windows 用户名去登远端
if ($script:ResolvedTarget -notmatch '@') {
    $u = if ($script:ResolvedHostname) { $script:ResolvedHostname } else { 'uos' }
    $script:ResolvedTarget = "$u@$($script:ResolvedTarget)"
}
$script:ResolvedDwellMs   = if ($cfg.PSObject.Properties.Name -contains 'DwellMs') { [int]$cfg.DwellMs } else { 0 }
$script:ResolvedClipsyncPort = if ($PSBoundParameters.ContainsKey('ClipsyncPort')) { $ClipsyncPort } else { 4243 }
$script:ResolvedClipsyncTarget = ($cfg.Target -replace '^.*@','' -replace ':.*$','')

Write-Host ""
Write-Host ("  Lan Mouse 一键连接   target=" + $script:ResolvedTarget + "  direction=" + $script:ResolvedDirection + "  port=" + $script:ResolvedPort) -ForegroundColor White
Write-Host ""

if ($Stop) { Stop-AllDaemons; return }

Write-Step "[1/4] 测试 SSH 连通性"
$sshTest = Test-Ssh
if (-not $sshTest.ok) {
    if ($sshTest.reason -eq 'auth') {
        # 公钥被拒：引导用密码登录一次、自动写公钥到远端
        if (-not (Enable-KeyAuth)) {
            Write-Err2 "SSH 公钥免密未能自动配置"
            Write-Sub ("（需要重新配置可加 -Setup 或删除 $ConfigPath）")
            exit 1
        }
    } else {
        Write-Err2 "SSH 连接失败到 $($script:ResolvedTarget)（$($sshTest.reason)）"
        Write-Sub "确认对方在线、网络通"
        if ($sshTest.output) { Write-Sub $sshTest.output.TrimEnd() }
        Write-Sub ("（需要重新配置可加 -Setup 或删除 $ConfigPath）")
        exit 1
    }
}
Write-OK "SSH 通"

Write-Step "[2/4] 探测远端状态"
$state = Probe-Remote
$verSuffix = if ($state.LAN_VER) { "  (" + $state.LAN_VER + ")" } else { "" }
Write-Sub ("架构          : " + $state.ARCH)
Write-Sub ("lan-mouse     : " + $state.INSTALLED + $verSuffix)
Write-Sub ("daemon 运行   : " + $state.RUNNING)
Write-Sub ("/dev/uinput   : " + $state.UINPUT_PERMS + "  writable=" + $state.UINPUT_OK)
Write-Sub ("WiFi powersave: " + $state.NM_POWERSAVE)
Write-Sub ("Win 源 IP (UOS 视角): " + $state.WIN_IP)

# 自动同步 Win LAN IP：UOS 收到的 SSH 源 IP = Win 给 UOS 发 UDP 包的真实源 IP；
# UOS toml [neighbor].ips 必须是这个值，否则收到 UDP 包时 client_manager 不认识就全 ignore。
if ($state.WIN_IP -and $state.WIN_IP -ne $script:ResolvedWinHostIp) {
    Write-Sub ("Win LAN IP 变化：" + $script:ResolvedWinHostIp + " -> " + $state.WIN_IP + "（自动更新）")
    $script:ResolvedWinHostIp = $state.WIN_IP
    $cfg.WinHostIp = $state.WIN_IP
    Save-Config $cfg
}

$needInstall = ($state.INSTALLED -ne 'yes') -or $Force
$needPerms   = ($state.UINPUT_OK -ne 'yes')

if ($needInstall) {
    Write-Step "[3/4] 远端首次安装"
    $secure = Read-Host -Prompt "    远端 sudo 密码 ($($script:ResolvedTarget))" -AsSecureString
    $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    $pass = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
    [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    Install-Remote -SudoPass $pass
    Start-RemoteDaemon
    Activate-Remote
    Start-RemoteClipsync
} elseif ($state.RUNNING -ne 'yes') {
    if ($needPerms) {
        Write-Step "[3a/4] 修复 /dev/uinput 权限（需要 sudo）"
        $secure = Read-Host -Prompt "    远端 sudo 密码" -AsSecureString
        $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
        $pass = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
        $fix = "chgrp input /dev/uinput; chmod 660 /dev/uinput; echo done"
        ($pass + "`n") | & ssh.exe $script:ResolvedTarget "sudo -S -p '' bash -c '$fix'" 2>&1 | Out-Null
        Write-OK "uinput 权限已修"
    }
    Write-Step "[3/4] 远端已装但未运行 — 同步 toml + 启动 daemon"
    [void](Ensure-RemoteConfig)
    Start-RemoteDaemon
    Activate-Remote
    Start-RemoteClipsync
} else {
    Write-Step "[3/4] 远端 daemon 已在运行 — 检查 toml + 重新激活"
    if (Ensure-RemoteConfig) {
        Write-Sub "UOS toml 内容已变 → 重启 UOS daemon"
        Start-RemoteDaemon
    }
    Activate-Remote
    Start-RemoteClipsync
}

Write-Step ("[4/4] Windows 端  dwell=" + $script:ResolvedDwellMs + "ms")
# 总是重启 Windows daemon —— 因为 dwell 通过 LAN_MOUSE_DWELL_MS 环境变量传入，
# 旧进程不会响应新值。重启代价 ~2 秒断流，可接受。
Start-LocalDaemon
Activate-Local
Start-LocalClipsync

Write-Host ""
Write-Host "  ====================  OK  ====================" -ForegroundColor Green
Write-Host ("  鼠标向 " + $script:ResolvedDirection + " 滑出 Windows 屏幕边缘 -> 越界到远端")
Write-Host  "  释放快捷键: 右 Alt"
Write-Host  "  停止两端  : .\connect.ps1 -Stop"
Write-Host  "  改方向    : .\connect.ps1 -Setup        (重跑向导)"
Write-Host  "                .\connect.ps1 -Direction right  (一次性覆盖)"
Write-Host  "  强制重装  : .\connect.ps1 -Force"
Write-Host ""
