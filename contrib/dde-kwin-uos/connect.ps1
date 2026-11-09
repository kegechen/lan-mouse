<#
.SYNOPSIS
    Lan Mouse KVM 一键部署/连接（patched-uinput 版）。

.DESCRIPTION
    通过 SSH 探测远端环境，按需安装/启动 lan-mouse，并启动 Windows 端 daemon
    完成对接。第一次运行会进入交互配置向导（询问 SSH 目标 + UOS 方向 + 主机 IP）
    并保存到 .connect-config.json，后续运行无需再问。
    .connect-config.json 支持多远端（Remotes 数组，DwellMs/AuthKey 顶层共享）：
    SSH 侧操作只针对本次选中的远端（-Remote 选择，缺省第 1 个），Windows
    config.toml 每次都写入全部远端的方向块并激活全部 client，重跑不会把
    双远端配置覆盖回单块。
    SSH 优先公钥免密；首次跑若未配免密会引导用密码登录一次、自动把本机公钥
    推到远端 authorized_keys，之后免密。安装时一次性 prompt sudo 密码。

.PARAMETER Target
    SSH 目标，格式 user@host（**必须带 user@**，否则会拿 Windows 用户名去登远端、SSH 直接失败）。
    传裸 IP 时脚本会用 -Hostname（默认 uos）补成 uos@host。覆盖配置文件。

.PARAMETER Remote
    配置文件存了多个远端时选择连接哪台：纯数字 = Remotes 下标（0 起），
    其它字符串 = 按 Target 子串或 Hostname 匹配。缺省第 1 个；传了 -Target 时
    自动按 host 匹配已有远端，匹配不到则视为新远端（首次连通后追加保存）。

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
    强制覆盖配置：本次目标的方向已被其它远端占用时，移除那些旧远端、由本次目标接管该方向
    （不加 -Force 会报"多个远端方向都是 'xxx'"退出）。不会触发重新编译。

.PARAMETER Rebuild
    强制重新打包源码 + 重新编译远端 lan-mouse（修复损坏安装、或拉新 patch 后更新）。

.PARAMETER Stop
    停止两端 daemon 后退出。

.PARAMETER ForgetSudo
    清除本远端已记住的 sudo 密码后退出（密码改了也会自动重新询问，一般不需要）。

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
    .\connect.ps1 -Remote 1
    多远端配置时连接 Remotes[1]（也可 -Remote uos-143 按 Hostname / IP 片段选）。

.EXAMPLE
    .\connect.ps1 -Direction right -DwellMs 200
    UOS 在 Windows 右边，鼠标顶住右边缘 200ms 才越界。

.EXAMPLE
    .\connect.ps1 -Stop
    停掉两端 daemon。

.EXAMPLE
    .\connect.ps1 -Rebuild
    远端重拉源码重编（修复损坏安装、或拉到了新 patch 后强制更新）。
#>
[CmdletBinding()]
param(
    [string]$Target,
    [string]$Remote,
    [ValidateSet('left','right','top','bottom')]
    [string]$Direction,
    [int]$Port,
    [string]$WinHostIp,
    [string]$Hostname,
    [string]$AuthKey,
    [int]$DwellMs,
    [int]$ClipsyncPort = 4243,
    [int]$FileDataPort = 4645,
    [switch]$NoClipsync,
    [switch]$DemoIndicator,
    [switch]$Setup,
    [switch]$Force,
    [switch]$Rebuild,
    [switch]$Stop,
    [switch]$ForgetSudo,
    [Alias('h')]
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
# 远端（Kylin/UOS，locale zh_CN.UTF-8）经 ssh 回传的中文是 UTF-8；PS 5.1 默认按控制台
# OEM 代码页(GBK/936)解码 ssh.exe 输出 → apt 等中文进度乱码（姝ｅ湪璇诲彇...）。把控制台输出
# 编码显式设为 UTF-8，让 UTF-8 字节正确还原。try/catch：句柄被重定向、无真实控制台时设置会抛，忽略。
try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}
$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
# 本仓库根 = 这个 patched lan-mouse fork 的源码树根，远端编译用的就是这棵树（git archive HEAD 打包）
$RepoRoot    = (Resolve-Path (Join-Path $ScriptDir '..\..')).Path
$WinBinDir   = Join-Path $ScriptDir 'bin'
$WinExe      = Join-Path $WinBinDir 'lan-mouse.exe'
$WinCfg      = Join-Path $ScriptDir 'config.toml'
$ConfigPath  = Join-Path $ScriptDir '.connect-config.json'
# sudo 密码缓存：按 SSH target 存，DPAPI 加密，仅本机当前 Windows 用户可解
$SudoCachePath = Join-Path $ScriptDir '.sudo-cache.json'

# ---------- output helpers ----------
function Write-Step($msg) { Write-Host ("`n[==] " + $msg) -ForegroundColor Cyan }
function Write-OK($msg)   { Write-Host ("  [OK] " + $msg) -ForegroundColor Green }
function Write-Sub($msg)  { Write-Host ("       " + $msg) -ForegroundColor DarkGray }
function Write-Warn2($msg){ Write-Host ("  [!!] " + $msg) -ForegroundColor Yellow }
function Write-Err2($msg) { Write-Host ("  [XX] " + $msg) -ForegroundColor Red }

# ---------- help ----------
function Show-Help {
    Write-Host ""
    Write-Host "  connect.ps1 — Lan Mouse KVM 一键部署/连接（多远端）" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  常用：" -ForegroundColor White
    Write-Host "    .\connect.ps1                        复用配置连接，激活全部已配置远端"
    Write-Host "    .\connect.ps1 -Target user@host       新增/切换远端（新主机自动分配空闲方向并存盘）"
    Write-Host "    .\connect.ps1 -Remote <n|片段>        多远端时按下标(0起) 或 hostname/IP 片段选一台做 SSH 侧操作"
    Write-Host "    .\connect.ps1 -Setup                  重跑交互配置向导"
    Write-Host "    .\connect.ps1 -Force                  强制覆盖配置（方向冲突时本次目标接管该方向）"
    Write-Host "    .\connect.ps1 -Rebuild                远端重新打包源码重编（修损坏安装/拉新 patch）"
    Write-Host "    .\connect.ps1 -Stop                   停止两端 daemon"
    Write-Host "    .\connect.ps1 -ForgetSudo             清除本远端记住的 sudo 密码"
    Write-Host "    .\connect.ps1 -Help                   显示本帮助"
    Write-Host ""
    Write-Host "  参数：" -ForegroundColor White
    Write-Host "    -Target user@host   SSH 目标，必须带 user@（裸 IP 用 -Hostname 兜底补全）"
    Write-Host "    -Direction v        方向 left/right/top/bottom；新增远端不给则自动挑空闲方向"
    Write-Host "    -Port n             lan-mouse UDP 端口（默认 4242）"
    Write-Host "    -WinHostIp ip       Win 在远端可见的 IP（默认 192.168.137.1，会按 SSH 源 IP 自动校正）"
    Write-Host "    -Hostname name      远端在 toml 里的显示名（默认 uos）"
    Write-Host "    -DwellMs n          鼠标顶边多少毫秒才越界（防误触，0=碰边即过）"
    Write-Host "    -AuthKey key        两端共享认证 key（默认自动生成并存盘，全部远端共享）"
    Write-Host "    -ClipsyncPort n     剪贴板同步 TCP 端口（默认 4243）"
    Write-Host "    -NoClipsync         关闭剪贴板同步"
    Write-Host "    -DemoIndicator      边缘 indicator 常显，方便远程截屏调试"
    Write-Host ""
    Write-Host "  新增一台远端（脚本自动加进 .connect-config.json，无需手动编辑）：" -ForegroundColor White
    Write-Host "    .\connect.ps1 -Target iflytek@192.168.137.33                  自动挑空闲方向"
    Write-Host "    .\connect.ps1 -Target iflytek@192.168.137.33 -Direction right  显式指定方向"
    Write-Host "    ！注意是 -Direction right，不能写 direction=right —— 后者会被当成 -Remote 的值。" -ForegroundColor Yellow
    Write-Host ""
    $doc = Load-ConfigDoc
    if ($doc -and $doc.Remotes) {
        Write-Host ("  当前已配置远端（" + $ConfigPath + "）：") -ForegroundColor White
        $i = 0
        foreach ($r in @($doc.Remotes)) {
            Write-Host ("    [" + $i + "] " + $r.Target + "  direction=" + $r.Direction)
            $i++
        }
        Write-Host ""
    }
}

# ---------- config load / save ----------
# 磁盘格式（多远端）：{ "Remotes": [ {Target,Direction,WinHostIp,Port,Hostname}, ... ],
#                      "DwellMs": N, "AuthKey": "..." } —— DwellMs/AuthKey 顶层共享
#                      （deploy-143.ps1 也按顶层读 AuthKey，别下移）。
# 旧扁平格式（远端字段直接在顶层）读入时自动升级；脚本内部照旧用扁平 $cfg，
# Save-Config 时回写进 Remotes[$script:RemoteIndex]、不动其它远端。
$script:ConfigDoc   = $null   # 磁盘上的完整文档（含全部远端）
$script:RemoteIndex = 0       # 本次会话操作的远端下标

function Normalize-Direction([string]$d) {
    # 配置文件是手工可编辑的，方向值没有 CLI ValidateSet 保护；up/down 意图无歧义直接归一
    $d = "$d".ToLower()
    if ($d -eq 'up')   { return 'top' }
    if ($d -eq 'down') { return 'bottom' }
    return $d
}

function Load-ConfigDoc {
    if (-not (Test-Path $ConfigPath)) { return $null }
    try { $raw = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json }
    catch { Write-Warn2 "配置文件损坏，将忽略: $ConfigPath"; return $null }
    if (($raw.PSObject.Properties.Name -contains 'Remotes') -and $raw.Remotes) { return $raw }
    if ($raw.PSObject.Properties.Name -contains 'Target') {
        # 旧扁平格式 → 升级成多远端文档（下次 Save-Config 落盘即完成迁移）
        $doc = [PSCustomObject]@{
            Remotes = @([PSCustomObject]@{
                Target    = $raw.Target
                Direction = $raw.Direction
                WinHostIp = $raw.WinHostIp
                Port      = $raw.Port
                Hostname  = $raw.Hostname
            })
        }
        foreach ($k in 'DwellMs','AuthKey') {
            if ($raw.PSObject.Properties.Name -contains $k) {
                $doc | Add-Member -NotePropertyName $k -NotePropertyValue $raw.$k
            }
        }
        return $doc
    }
    Write-Warn2 "配置文件缺 Remotes/Target 字段，将忽略: $ConfigPath"
    return $null
}
function Select-RemoteIndex {
    # -Remote 纯数字 = Remotes 下标（0 起）；其它字符串 = Target 子串 / Hostname 全等匹配。
    # 没给 -Remote 但给了 -Target：按 host 匹配已有远端，匹配不到返回 Count（= 追加新远端）。
    # 都没给：第 1 个。
    param($Doc, [string]$Selector, [string]$TargetOverride)
    $remotes = @($Doc.Remotes)
    if ($Selector) {
        if ($Selector -match '^\d+$') {
            if ([int]$Selector -lt $remotes.Count) { return [int]$Selector }
            Write-Err2 "-Remote $Selector 超出范围（配置里共 $($remotes.Count) 个远端，下标从 0 起）"
        } else {
            for ($i = 0; $i -lt $remotes.Count; $i++) {
                if (("$($remotes[$i].Target)" -like "*$Selector*") -or ("$($remotes[$i].Hostname)" -eq $Selector)) { return $i }
            }
            Write-Err2 "-Remote '$Selector' 没匹配到任何远端"
        }
        for ($i = 0; $i -lt $remotes.Count; $i++) {
            Write-Sub ("  [$i] " + $remotes[$i].Target + "  direction=" + $remotes[$i].Direction)
        }
        exit 1
    }
    if ($TargetOverride) {
        $tHost = $TargetOverride -replace '^.*@','' -replace ':.*$',''
        for ($i = 0; $i -lt $remotes.Count; $i++) {
            if (("$($remotes[$i].Target)" -replace '^.*@','' -replace ':.*$','') -eq $tHost) { return $i }
        }
        return $remotes.Count
    }
    return 0
}
function Resolve-ExistingConfig {
    # 读文档 + 选远端 + 展平成脚本内部用的 $cfg 形状；没有可用配置时返回 $null
    $script:ConfigDoc = Load-ConfigDoc
    if (-not $script:ConfigDoc) { return $null }
    $remotes = @($script:ConfigDoc.Remotes)
    $script:RemoteIndex = Select-RemoteIndex -Doc $script:ConfigDoc -Selector $Remote -TargetOverride $Target
    if ($script:RemoteIndex -ge $remotes.Count) {
        Write-Sub "-Target 未匹配到已有远端 → 按新远端处理（首次连通后追加进 $ConfigPath）"
        # 自动挑一个没被已有远端占用的方向：新远端默认硬编码 'left' 会跟已有 left 远端撞车，
        # Assert-DirectionUnique 直接报错（用户就撞在这）。改成挑空闲方向，无需 -Direction 也能加。
        $used = @{}
        foreach ($r in $remotes) { $used[(Normalize-Direction $r.Direction)] = $true }
        $freeDir = @('left','right','top','bottom') | Where-Object { -not $used.ContainsKey($_) } | Select-Object -First 1
        if (-not $freeDir) { $freeDir = 'left' }  # 四个方向占满 → 留给 Assert-DirectionUnique 报错引导
        # 共享字段（Win 主机 IP / 端口）沿用第 1 个已有远端；方向单独挑；-Direction 参数后续仍可覆盖
        $proto = if ($remotes.Count -gt 0) { $remotes[0] } else { $null }
        $sel = [PSCustomObject]@{
            Target    = $Target
            Direction = $freeDir
            WinHostIp = if ($proto -and $proto.WinHostIp) { $proto.WinHostIp } else { '192.168.137.1' }
            Port      = if ($proto -and $proto.Port)      { [int]$proto.Port } else { 4242 }
            Hostname  = 'uos'
        }
        Write-Sub ("  新远端自动分配方向 = " + $freeDir + "（如需其它方向：-Direction left/right/top/bottom）")
    } else {
        $sel = $remotes[$script:RemoteIndex]
        if ($remotes.Count -gt 1) {
            Write-Sub ("使用远端 [" + $script:RemoteIndex + "] " + $sel.Target + "（配置共 " + $remotes.Count + " 个，-Remote 下标/名称切换）")
        }
    }
    $flat = [PSCustomObject]@{
        Target    = $sel.Target
        Direction = $sel.Direction
        WinHostIp = $sel.WinHostIp
        Port      = $sel.Port
        Hostname  = $sel.Hostname
    }
    foreach ($k in 'DwellMs','AuthKey') {
        if ($script:ConfigDoc.PSObject.Properties.Name -contains $k) {
            $flat | Add-Member -NotePropertyName $k -NotePropertyValue $script:ConfigDoc.$k
        }
    }
    return $flat
}
function Save-Config($cfg) {
    # 扁平 $cfg 回写：远端字段进 Remotes[$script:RemoteIndex]，DwellMs/AuthKey 留顶层共享
    $remotes = @()
    if ($script:ConfigDoc) { $remotes = @($script:ConfigDoc.Remotes) }
    $entry = [PSCustomObject]@{
        Target    = $cfg.Target
        Direction = $cfg.Direction
        WinHostIp = $cfg.WinHostIp
        Port      = $cfg.Port
        Hostname  = $cfg.Hostname
    }
    if ($script:RemoteIndex -lt $remotes.Count) { $remotes[$script:RemoteIndex] = $entry }
    else { $remotes = @($remotes) + @($entry); $script:RemoteIndex = $remotes.Count - 1 }
    $doc = [PSCustomObject]@{ Remotes = @($remotes) }
    foreach ($k in 'DwellMs','AuthKey') {
        if (($cfg.PSObject.Properties.Name -contains $k) -and $null -ne $cfg.$k) {
            $doc | Add-Member -NotePropertyName $k -NotePropertyValue $cfg.$k
        }
    }
    $doc | ConvertTo-Json -Depth 5 | Set-Content -Path $ConfigPath -Encoding UTF8
    $script:ConfigDoc = $doc
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
    # 顶层共享字段随向导保留，否则每跑一次向导 AuthKey 就重新生成、DwellMs 丢失
    foreach ($k in 'DwellMs','AuthKey') {
        if ($Existing -and ($Existing.PSObject.Properties.Name -contains $k) -and $null -ne $Existing.$k) {
            $newCfg | Add-Member -NotePropertyName $k -NotePropertyValue $Existing.$k
        }
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
        # 本文件是 CRLF，here-string 内容带 \r；远端 Linux bash 对 `then\r` 直接
        # 语法报错、整段脚本零输出（Probe-Remote 全空就是这么来的），统一转 LF
        $Stdin = $Stdin -replace "`r`n", "`n"
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

# ---------- sudo password cache ----------
function Read-SudoCache {
    $h = @{}
    if (Test-Path -LiteralPath $SudoCachePath) {
        try {
            $raw = Get-Content -LiteralPath $SudoCachePath -Raw -Encoding UTF8 | ConvertFrom-Json
            foreach ($p in $raw.PSObject.Properties) { $h[$p.Name] = "$($p.Value)" }
        } catch { Write-Warn2 "sudo 密码缓存损坏，忽略: $SudoCachePath" }
    }
    return $h
}
function Write-SudoCache($h) {
    $obj = [PSCustomObject]@{}
    foreach ($k in $h.Keys) { $obj | Add-Member -NotePropertyName $k -NotePropertyValue $h[$k] }
    $obj | ConvertTo-Json | Set-Content -LiteralPath $SudoCachePath -Encoding UTF8
}
# 直接用 .NET DPAPI（CurrentUser 范围），不依赖 ConvertTo/From-SecureString —— 某些环境下
# Microsoft.PowerShell.Security 模块自动加载会失败（PSModulePath 被 pwsh7 污染）
function Protect-Str([string]$s) {
    try { Add-Type -AssemblyName System.Security -ErrorAction SilentlyContinue } catch {}
    $b = [System.Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes($s), $null, 'CurrentUser')
    return [Convert]::ToBase64String($b)
}
function Unprotect-Str([string]$s) {
    try { Add-Type -AssemblyName System.Security -ErrorAction SilentlyContinue } catch {}
    $b = [System.Security.Cryptography.ProtectedData]::Unprotect([Convert]::FromBase64String($s), $null, 'CurrentUser')
    return [Text.Encoding]::UTF8.GetString($b)
}
function Test-SudoPass([string]$Pass) {
    # -k 丢弃远端 sudo 时间戳，确保真的校验了这个密码（否则缓存凭据会让错密码也"通过"）
    $out = ($Pass + "`n") | & ssh.exe @script:SshOpts $script:ResolvedTarget "sudo -S -k -p '' true && echo SUDO_OK" 2>&1
    return (($out | Out-String) -match 'SUDO_OK')
}
function Get-SudoPass {
    # 同一远端只问一次：先试缓存（远端校验通过才用），失效/没有则 prompt（最多 3 次），校验通过后存盘
    $key   = $script:ResolvedTarget
    $cache = Read-SudoCache
    if ($cache.ContainsKey($key)) {
        $plain = $null
        try { $plain = Unprotect-Str $cache[$key] } catch {}
        if ($plain -and (Test-SudoPass $plain)) {
            Write-Sub "使用已记住的 sudo 密码（$key；清除: -ForgetSudo）"
            return $plain
        }
        Write-Warn2 "已记住的 sudo 密码失效，重新输入"
    }
    for ($i = 0; $i -lt 3; $i++) {
        $secure = Read-Host -Prompt "    远端 sudo 密码 ($key)" -AsSecureString
        $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
        $plain = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
        if (Test-SudoPass $plain) {
            $cache[$key] = Protect-Str $plain
            Write-SudoCache $cache
            Write-Sub "sudo 密码已记住（DPAPI 加密: $SudoCachePath）"
            return $plain
        }
        Write-Warn2 "sudo 密码错误（或该用户无 sudo 权限）"
    }
    Write-Err2 "sudo 密码校验失败 3 次，退出"
    exit 1
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
    $script:ProbeRawOut = ($out | Out-String).Trim()
    $h = @{}
    foreach ($line in ($out -split "`r?`n")) {
        if ($line -match '^([A-Z_]+)=(.*)$') { $h[$matches[1]] = $matches[2].Trim() }
    }
    return $h
}

# ---------- install on remote ----------
function Get-SourceTarball {
    # 源码包优先级：
    #   1. 脚本所在源码树（原始位置 contrib/dde-kwin-uos/）
    #   2. build.ps1 部署时写下的 .source-repo（D:\tools\lan-mouse 这种独立部署目录不是 git 仓库，
    #      之前直接掉到第 3 步、用一个几个月前的旧缓存包重编 —— 重编 "不覆盖" 就是这么来的）
    #   3. $ScriptDir\lan-mouse-src.tar.gz 缓存包（build.ps1 每次部署刷新）
    # git 打包用 `git stash create` 把工作区未提交改动也带上（干净时为空 → 退回 HEAD）
    $tarball = Join-Path $env:TEMP 'lan-mouse-src.tar.gz'
    $repos = @($RepoRoot)
    $srcFile = Join-Path $ScriptDir '.source-repo'
    if (Test-Path -LiteralPath $srcFile) {
        $p = (Get-Content -LiteralPath $srcFile -Raw -Encoding UTF8).Trim()
        if ($p) { $repos += $p }
    }
    foreach ($repo in $repos) {
        if (-not (Test-Path -LiteralPath (Join-Path $repo '.git'))) { continue }
        Remove-Item -LiteralPath $tarball -ErrorAction SilentlyContinue
        $ref = (& git -C $repo stash create 2>$null | Out-String).Trim()
        $label = if ($ref) { 'HEAD + 未提交改动' } else { 'HEAD' }
        if (-not $ref) { $ref = 'HEAD' }
        & git -C $repo archive --format=tar.gz -o $tarball $ref 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $tarball)) {
            Write-Sub "打包源码树 $repo ($label)..."
            return $tarball
        }
    }
    $cached = Join-Path $ScriptDir 'lan-mouse-src.tar.gz'
    if (Test-Path -LiteralPath $cached) {
        $age = (Get-Item -LiteralPath $cached).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
        Write-Warn2 "找不到 git 源码树，使用缓存源码包（$age）: $cached"
        Write-Sub "（缓存包可能过旧；在源码仓库重跑 build.ps1 可刷新它和 .source-repo）"
        return $cached
    }
    Write-Err2 "拿不到源码包：$RepoRoot 不是 git 仓库，且缓存包不存在: $cached"
    Write-Sub "从任一已装机器生成缓存包："
    Write-Sub ("  ssh <user@host> ""tar -C ~/src/lan-mouse --exclude=./target --exclude=./.git -czf -"" > ""$cached""")
    exit 1
}
function Install-Remote {
    param([string]$SudoPass)

    $opp = @{ left='right'; right='left'; top='bottom'; bottom='top' }[$script:ResolvedDirection]

    # 把整棵 patched 源码树打包丢过去，远端解压即编译——比早期 "git clone v0.10.0 + cp 5 个 patch"
    # 健壮得多：不会因为 patch 列表漂移漏文件（v0.10.0..HEAD 实际改了 9 个源码文件），
    # 远端也不再依赖 git/clone 网络可达
    $tarball = Get-SourceTarball
    $sizeKb = [math]::Round((Get-Item $tarball).Length / 1KB, 1)
    Write-Sub "上传源码包 ($sizeKb KB)..."
    Copy-ToRemote -Local $tarball -Remote '/tmp/lan-mouse-src.tar.gz'

    Write-Sub "上传 UOS config.toml..."
    $uosCfg = @"
# generated by connect.ps1
release_bind = ["KeyRightalt"]
port = $($script:ResolvedPort)
authentication_key = "$($script:ResolvedAuthKey)"

[$opp]
hostname = "win-host"
ips = ["$($script:ResolvedWinHostIp)"]
port = $($script:ResolvedPort)
activate_on_startup = true
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

echo "[3/8] apt deps"
# build-essential: 提供 gcc/make/libc6-dev，cargo 编含 C 绑定的 crate (ring/openssl-sys/cc) 时必需，
#   Kylin/UOS server 最小化镜像或精简容器里默认没有，桌面版一般有，无脑装更稳。
# libudev-dev/libxkbcommon-dev: 通过 pkg-config 被 libudev-sys/xkbcommon 找；Kylin/UOS 默认只装
#   runtime libudev1/libxkbcommon0，开发头不会自动来。
# pkg-config: 上面两个 -dev 包都靠它定位 .pc 文件。
# 逐个装、失败不中止：UOS 专业版未激活时官方源对部分包返回 401 Unauthorized（实测 libudev-dev），
# 整条 apt-get 失败会让 set -e 直接 abort。缺 libudev-dev 时 install-user.sh 会用系统
# libudev.so.1 生成 pkg-config shim 兜底；真缺 gcc 等硬依赖会在 cargo 编译阶段报错。
for PKG in build-essential pkg-config libudev-dev libxkbcommon-dev; do
    if dpkg -s "$PKG" >/dev/null 2>&1; then continue; fi
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$PKG" 2>&1 | tail -3 \
        || true
    dpkg -s "$PKG" >/dev/null 2>&1 || echo "  [warn] apt 装不上 $PKG（源不可达/未授权？），继续"
done
# libx11-dev/libxtst-dev 在某些 UOS 镜像上与系统包冲突，失败可忽略（uinput 后端不强制依赖）
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

echo "[6/8] deploy lan-mouse source tree"
# 直接解压 Win 端推过来的 git archive HEAD 源码包；之前是 "git clone v0.10.0 + cp 几个 patch 文件覆盖"，
# patch 列表与本仓 v0.10.0..HEAD 漂移导致 src/server/* 的近期 ping 修复永远漏掉。
rm -rf "$HOME/src/lan-mouse"
mkdir -p "$HOME/src/lan-mouse"
tar xzf /tmp/lan-mouse-src.tar.gz -C "$HOME/src/lan-mouse"

echo "[7/8] cargo install (1-3 minutes on first build)"
# libudev-dev 没装上（UOS 源 401 等）时：用运行时 libudev.so.1 造一个 .pc + libudev.so 软链，
# libudev-sys 只需要 pkg-config 能给出 -ludev，链接期找到 .so 即可（不用头文件）
if ! pkg-config --exists libudev 2>/dev/null; then
    UDEV_SO=$(ls /lib/*-linux-gnu/libudev.so.1 /usr/lib/*-linux-gnu/libudev.so.1 /usr/lib64/libudev.so.1 2>/dev/null | head -1)
    if [ -n "$UDEV_SO" ]; then
        SHIM="$HOME/.local/lib/lan-mouse-shim"
        mkdir -p "$SHIM/pkgconfig"
        ln -sf "$UDEV_SO" "$SHIM/libudev.so"
        printf 'libdir=%s\n\nName: libudev\nDescription: libudev shim (connect.ps1)\nVersion: 241\nLibs: -L${libdir} -ludev\nCflags:\n' "$SHIM" > "$SHIM/pkgconfig/libudev.pc"
        export PKG_CONFIG_PATH="$SHIM/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        echo "  (无 libudev-dev，使用 shim: $UDEV_SO)"
    else
        echo "  [warn] 找不到 libudev.so.1，编译可能失败"
    fi
fi
cd "$HOME/src/lan-mouse"
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
    Write-Step "启动远端 clipsync (listen :$($script:ResolvedClipsyncPort), 文件数据端口 :$($script:ResolvedFileDataPort))"
    $port     = $script:ResolvedClipsyncPort
    $fileport = $script:ResolvedFileDataPort
    $key      = $script:ResolvedAuthKey

    # 固化：把本机暂存的 aarch64 静态 clipsync 推到远端 ~/.cargo/bin/clipsync。
    # 远端无 rust，原“远端从源码 build”路径已失效，故改推预编译静态二进制（bin\clipsync-linux-aarch64）。
    # 仅当远端是 aarch64、本地暂存存在、且 SHA 与远端不一致时才推（幂等、省流量）。
    $staged = Join-Path $WinBinDir 'clipsync-linux-aarch64'
    if (Test-Path $staged) {
        $arch = (Invoke-Ssh "uname -m" | Out-String).Trim()
        if ($arch -match 'aarch64') {
            $localSha  = (Get-FileHash $staged -Algorithm SHA256).Hash.ToLower()
            $remoteSha = (Invoke-Ssh "sha256sum ~/.cargo/bin/clipsync 2>/dev/null | cut -d' ' -f1" | Out-String).Trim().ToLower()
            if ($localSha -ne $remoteSha) {
                Write-Sub "推送 aarch64 clipsync -> 远端 ~/.cargo/bin/clipsync"
                Invoke-Ssh "mkdir -p ~/.cargo/bin" | Out-Null
                & scp.exe -q @($script:SshOpts) $staged "$($script:ResolvedTarget):.cargo/bin/clipsync" 2>&1 | Out-Null
                Invoke-Ssh "chmod +x ~/.cargo/bin/clipsync" | Out-Null
            }
        } else {
            Write-Warn2 "远端 arch=$arch 非 aarch64，跳过 clipsync 二进制推送（文件功能需匹配架构二进制）"
        }
    }

    # 用 here-string + bash -s 方式喂脚本，避免 ssh 单行命令对 ; & 的 quote bug。
    # DISPLAY/XAUTHORITY 必须显式 export（ssh non-interactive 不继承图形 session）——
    # detect_x11 靠 xclip 读 X11 剪贴板检测文件复制；--data-key/--file-data-port 开启文件传输。
    $script = @"
pkill -f 'cargo/bin/clipsync' 2>/dev/null || true
sleep 1
rm -f /tmp/clipsync.log
if [ ! -x "`$HOME/.cargo/bin/clipsync" ]; then
    echo NOT_INSTALLED
    exit 0
fi
command -v xclip >/dev/null 2>&1 || echo NO_XCLIP
export WAYLAND_DISPLAY=wayland-0
export XDG_RUNTIME_DIR=/run/user/1000
export DISPLAY=:0
export XAUTHORITY=`$HOME/.Xauthority
nohup "`$HOME/.cargo/bin/clipsync" --listen 0.0.0.0:$port --data-key '$key' --file-data-port $fileport > /tmp/clipsync.log 2>&1 < /dev/null &
disown
sleep 1
pgrep -f cargo/bin/clipsync >/dev/null && echo OK || echo FAIL
"@
    $out = Invoke-Ssh "bash -s" -Stdin $script
    $joined = ($out -join "`n")
    if ($joined -match 'NO_XCLIP') {
        Write-Warn2 "远端未装 xclip — 文件复制检测会失效！远端执行: sudo apt install -y xclip"
    }
    if ($joined -match 'NOT_INSTALLED') {
        Write-Warn2 "远端无 clipsync 二进制（本地暂存 bin\clipsync-linux-aarch64 缺失？）"
    } elseif ($joined -match 'OK') {
        Write-OK "远端 clipsync 已 listen :$port (文件模式, 数据端口 :$fileport)"
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
authentication_key = "$($script:ResolvedAuthKey)"

[$opp]
hostname = "win-host"
ips = ["$($script:ResolvedWinHostIp)"]
port = $($script:ResolvedPort)
activate_on_startup = true
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
    $script = $script -replace "`r`n", "`n"   # 同 Invoke-Ssh：喂远端 bash 的内容必须 LF
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
function Get-ResolvedRemotes {
    # 配置里的全部远端（选中的那个换成本次 resolved 值；-Target 新主机场景追加到末尾），
    # 供 Windows config.toml 生成全部方向块 —— 重跑不再把多远端覆盖回单块
    $list = @()
    if ($script:ConfigDoc) {
        foreach ($r in @($script:ConfigDoc.Remotes)) {
            if ($r.PSObject.Properties.Name -contains 'Direction') {
                $r.Direction = Normalize-Direction $r.Direction
            }
            $list += ,$r
        }
    }
    $sel = [PSCustomObject]@{
        Target    = $script:ResolvedTarget
        Direction = $script:ResolvedDirection
        WinHostIp = $script:ResolvedWinHostIp
        Port      = $script:ResolvedPort
        Hostname  = $script:ResolvedHostname
    }
    if ($script:RemoteIndex -lt $list.Count) { $list[$script:RemoteIndex] = $sel }
    else { $list += ,$sel }
    return ,$list
}
function Assert-DirectionUnique {
    # 两个远端同方向会生成重复 TOML 表，Windows daemon 起不来。
    # MAIN 在 banner 后就调（别等远端装完 2-4 分钟才在 [4/4] 报错）；Ensure-LocalConfig 兜底再查。
    $remotes = Get-ResolvedRemotes
    $seen = @{}
    foreach ($r in $remotes) {
        $d = "$($r.Direction)"
        if (@('left','right','top','bottom') -notcontains $d) {
            Write-Err2 "远端 $($r.Target) 的方向 '$d' 无效（合法值 left/right/top/bottom），检查 $ConfigPath"
            exit 1
        }
        if ($seen.ContainsKey($d)) {
            Write-Err2 "多个远端方向都是 '$d'（$($seen[$d]) 和 $($r.Target)），Windows config 无法生成"
            Write-Sub "用 -Direction 给本次目标指定空闲方向（left/right/top/bottom），或 -Setup 重排"
            exit 1
        }
        $seen[$d] = $r.Target
    }
}
function Ensure-LocalConfig {
    Assert-DirectionUnique
    $remotes = Get-ResolvedRemotes
    $localCfg = @"
# generated by connect.ps1
release_bind = ["KeyRightalt"]
port = $($script:ResolvedPort)
authentication_key = "$($script:ResolvedAuthKey)"
"@
    foreach ($r in $remotes) {
        $ip = "$($r.Target)" -replace '^.*@','' -replace ':.*$',''
        $localCfg += @"


[$($r.Direction)]
hostname = "$($r.Hostname)"
ips = ["$ip"]
port = $([int]$r.Port)
activate_on_startup = true
"@
    }
    [System.IO.File]::WriteAllText($WinCfg, $localCfg, [System.Text.UTF8Encoding]::new($false))
    Write-Sub "Windows config: $WinCfg（$($remotes.Count) 个远端方向块）"
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
    # 注意别用 @(...) 包 —— 函数 `return ,$list` 已保证数组，@() 会再包一层使 Count 恒为 1
    $n = (Get-ResolvedRemotes).Count
    Write-Step "激活 Windows client 0..$($n - 1)"
    $acts = ((0..($n - 1)) | ForEach-Object { "activate $_" }) -join "`n"
    "$acts`nlist" | & $WinExe -f cli -c $WinCfg 2>&1 | Out-Null
    Write-OK "Windows client 已激活（$n 个）"
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
                      '--connect', "$($script:ResolvedClipsyncTarget):$($script:ResolvedClipsyncPort)",
                      '--data-key', $script:ResolvedAuthKey,
                      '--file-data-port', "$($script:ResolvedFileDataPort)"
                  ) `
                  -WindowStyle Hidden `
                  -RedirectStandardOutput $stdoutLog `
                  -RedirectStandardError  $stderrLog | Out-Null
    Start-Sleep -Milliseconds 500
    if (Get-Process clipsync -ErrorAction SilentlyContinue) {
        Write-OK "Windows clipsync 启动 (文本+文件模式, 数据端口 :$($script:ResolvedFileDataPort))"
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
if ($Help) { Show-Help; return }

$existing = Resolve-ExistingConfig

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
$script:ResolvedDirection = Normalize-Direction $cfg.Direction
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
$script:ResolvedFileDataPort = if ($PSBoundParameters.ContainsKey('FileDataPort')) { $FileDataPort } else { 4645 }
$script:ResolvedClipsyncTarget = ($cfg.Target -replace '^.*@','' -replace ':.*$','')

# 认证预共享 key（HMAC-SHA256）：两端 config 必须写同一个值，否则新版二进制 fail-closed
# 全丢包。优先 -AuthKey 参数；否则复用 .connect-config.json 里存的；都没有则随机生成
# 32 字符并存盘（首次），之后每次复用同一 key。
if ($PSBoundParameters.ContainsKey('AuthKey') -and -not [string]::IsNullOrWhiteSpace($AuthKey)) {
    if ($cfg.PSObject.Properties.Name -contains 'AuthKey') { $cfg.AuthKey = $AuthKey }
    else { $cfg | Add-Member -NotePropertyName AuthKey -NotePropertyValue $AuthKey -Force }
    Save-Config $cfg
}
if (-not ($cfg.PSObject.Properties.Name -contains 'AuthKey') -or [string]::IsNullOrWhiteSpace($cfg.AuthKey)) {
    $authChars = @((48..57) + (65..90) + (97..122))
    $genKey = -join (1..32 | ForEach-Object { [char]($authChars | Get-Random) })
    if ($cfg.PSObject.Properties.Name -contains 'AuthKey') { $cfg.AuthKey = $genKey }
    else { $cfg | Add-Member -NotePropertyName AuthKey -NotePropertyValue $genKey -Force }
    Save-Config $cfg
}
$script:ResolvedAuthKey = $cfg.AuthKey

Write-Host ""
Write-Host ("  Lan Mouse 一键连接   target=" + $script:ResolvedTarget + "  direction=" + $script:ResolvedDirection + "  port=" + $script:ResolvedPort) -ForegroundColor White
Write-Host ""

if ($Stop) { Stop-AllDaemons; return }
if ($ForgetSudo) {
    $c = Read-SudoCache
    if ($c.ContainsKey($script:ResolvedTarget)) {
        $c.Remove($script:ResolvedTarget); Write-SudoCache $c
        Write-OK "已清除 $($script:ResolvedTarget) 的 sudo 密码"
    } else { Write-Sub "$($script:ResolvedTarget) 没有记住的 sudo 密码" }
    return
}

# -Force：本次目标的方向被其它远端占着时，直接把那些旧远端从配置里踢掉（本次目标接管该方向），
# 而不是让 Assert-DirectionUnique 报 "多个远端方向都是 'left'" 退出
if ($Force -and $script:ConfigDoc) {
    $old = @($script:ConfigDoc.Remotes)
    $keep = @(); $newIdx = $script:RemoteIndex
    for ($i = 0; $i -lt $old.Count; $i++) {
        if ($i -ne $script:RemoteIndex -and (Normalize-Direction $old[$i].Direction) -eq $script:ResolvedDirection) {
            Write-Warn2 ("-Force: 远端 " + $old[$i].Target + " 原占方向 '" + $script:ResolvedDirection + "'，连通后将从配置移除，由 " + $script:ResolvedTarget + " 接管")
            if ($i -lt $script:RemoteIndex) { $newIdx-- }
        } else { $keep += ,$old[$i] }
    }
    if ($keep.Count -ne $old.Count) {
        $script:ConfigDoc.Remotes = @($keep)
        $script:RemoteIndex = $newIdx
        $script:ForcePruned = $true   # 先只改内存，SSH 连通后再落盘：目标连不上时不丢旧配置
    }
}

Assert-DirectionUnique

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
# 防呆：SSH 通但探测一个字段都没有（远端 bash 报错/输出被吞）时立即停 ——
# 否则会被误判成"未安装"，走进首次安装 + sudo 密码 + git archive 的死胡同
if (-not $state.ARCH) {
    Write-Err2 "远端探测无输出（SSH 通但 bash 脚本没返回字段），原始输出："
    Write-Sub ($script:ProbeRawOut)
    exit 1
}
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

# 新远端（-Target 没匹配到配置里任何一台 / 首次跑）连通后落盘保存
$savedCount = 0
if ($script:ConfigDoc) { $savedCount = @($script:ConfigDoc.Remotes).Count }
if ($script:RemoteIndex -ge $savedCount) {
    Write-Sub "新远端首次连通 → 保存进 $ConfigPath"
    Save-Config $cfg
} elseif ($script:ForcePruned) {
    Save-Config $cfg
    Write-Sub "已更新 $ConfigPath（移除被接管方向的旧远端）"
}

$needInstall = ($state.INSTALLED -ne 'yes') -or $Rebuild
$needPerms   = ($state.UINPUT_OK -ne 'yes')

if ($needInstall) {
    Write-Step "[3/4] 远端首次安装"
    $pass = Get-SudoPass
    Install-Remote -SudoPass $pass
    Start-RemoteDaemon
    Activate-Remote
    Start-RemoteClipsync
} elseif ($state.RUNNING -ne 'yes') {
    if ($needPerms) {
        Write-Step "[3a/4] 修复 /dev/uinput 权限（需要 sudo）"
        $pass = Get-SudoPass
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
Write-Host  "  强制重编  : .\connect.ps1 -Rebuild"
Write-Host  "  覆盖配置  : .\connect.ps1 -Target user@host -Direction left -Force"
Write-Host  "  多远端    : .\connect.ps1 -Remote 1     (或 -Remote <hostname/IP 片段>)"
Write-Host ""
