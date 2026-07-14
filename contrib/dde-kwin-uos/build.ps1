<#
.SYNOPSIS
    一键 build + 部署 lan-mouse + clipsync 到本地，准备好 connect.ps1 即可使用。

.DESCRIPTION
    场景：克隆这个 fork 分支后第一次配置；或换新机后重新部署。

    脚本流程：
    1. 检测 Rust（cargo），未装则下载 rustup-init.exe 装最小 stable 工具链
    2. 设代理 + crates.io 国内镜像（写 ~/.cargo/config.toml）
    3. cargo build --release --no-default-features  → patched lan-mouse.exe
    4. cargo build --release  在 contrib/dde-kwin-uos/clipsync/ → clipsync.exe
    5. 部署到 -DeployDir（默认 D:\tools\lan-mouse）：
       - bin/lan-mouse.exe + bin/clipsync.exe
       - connect.ps1 + USAGE.md
       - patches/ 归档（重装 UOS 端时用）

.PARAMETER DeployDir
    部署根目录。默认 D:\tools\lan-mouse。

.PARAMETER Proxy
    HTTP/HTTPS 代理（rustup 下载 + cargo 拉 crate 走它）。默认 http://127.0.0.1:10808。

.PARAMETER SkipProxy
    跳过代理设置（已有可用网络则用这个）。

.PARAMETER NoDeploy
    只编译不部署。binary 留在 target/release/。

.EXAMPLE
    .\build.ps1
    默认参数：用 127.0.0.1:10808 代理 + 部署到 D:\tools\lan-mouse

.EXAMPLE
    .\build.ps1 -DeployDir E:\kvm -SkipProxy
    部署到 E:\kvm，不用代理（直连 crates.io）

.EXAMPLE
    .\build.ps1 -NoDeploy
    只编译验证，不复制 binary。
#>
[CmdletBinding()]
param(
    [string]$DeployDir = 'D:\tools\lan-mouse',
    [string]$Proxy     = 'http://127.0.0.1:10808',
    [switch]$SkipProxy,
    [switch]$NoDeploy
)

$ErrorActionPreference = 'Stop'
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = (Resolve-Path (Join-Path $ScriptDir '..\..')).Path

function Write-H($msg)   { Write-Host ("`n=== " + $msg + " ===") -ForegroundColor Cyan }
function Write-OK($msg)  { Write-Host ("[OK] " + $msg) -ForegroundColor Green }
function Write-Wm($msg)  { Write-Host ("[!!] " + $msg) -ForegroundColor Yellow }

Write-Host ""
Write-Host "  lan-mouse + clipsync build & deploy" -ForegroundColor White
Write-Host ("  repo root : " + $RepoRoot)
Write-Host ("  deploy to : " + $DeployDir)
Write-Host ("  proxy     : " + $(if ($SkipProxy) { '(skipped)' } else { $Proxy }))

# ---------- 1. 检测 / 安装 Rust ----------
Write-H "[1/5] Rust toolchain"
$cargoBin = "$env:USERPROFILE\.cargo\bin"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    if (Test-Path "$cargoBin\cargo.exe") {
        $env:PATH = "$cargoBin;$env:PATH"
        Write-OK "cargo found at $cargoBin (added to PATH)"
    } else {
        Write-Wm "cargo not found, installing rustup..."
        if (-not $SkipProxy) {
            $env:HTTPS_PROXY = $Proxy
            $env:HTTP_PROXY  = $Proxy
        }
        $rustupExe = Join-Path $env:TEMP 'rustup-init.exe'
        Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $rustupExe -TimeoutSec 120
        $env:RUSTUP_DIST_SERVER  = 'https://rsproxy.cn'
        $env:RUSTUP_UPDATE_ROOT  = 'https://rsproxy.cn/rustup'
        & $rustupExe -y --profile minimal --default-toolchain stable --no-modify-path
        if ($LASTEXITCODE -ne 0) { throw "rustup-init failed (exit $LASTEXITCODE)" }
        $env:PATH = "$cargoBin;$env:PATH"
        Write-OK "rustup installed"
    }
}
Write-OK ("cargo: " + ((& cargo --version) -join ''))

# ---------- 2. cargo 代理 + 镜像 ----------
Write-H "[2/5] cargo proxy + crates.io mirror"
if (-not $SkipProxy) {
    $env:HTTPS_PROXY = $Proxy
    $env:HTTP_PROXY  = $Proxy
}
$cargoCfg = "$env:USERPROFILE\.cargo\config.toml"
if (-not (Test-Path $cargoCfg)) {
    if (-not (Test-Path "$env:USERPROFILE\.cargo")) {
        New-Item -ItemType Directory "$env:USERPROFILE\.cargo" -Force | Out-Null
    }
    $body = "[http]`nproxy = `"$Proxy`"`n[https]`nproxy = `"$Proxy`"`n[net]`ngit-fetch-with-cli = true`n[source.crates-io]`nreplace-with = `"rsproxy-sparse`"`n[source.rsproxy-sparse]`nregistry = `"sparse+https://rsproxy.cn/index/`"`n"
    [System.IO.File]::WriteAllText($cargoCfg, $body, [System.Text.UTF8Encoding]::new($false))
    Write-OK "wrote $cargoCfg"
} else {
    Write-OK "$cargoCfg already exists, leaving as is"
}

# ---------- 3. 编 lan-mouse ----------
Write-H "[3/5] building lan-mouse (release, no default features)"
Push-Location $RepoRoot
try {
    & cargo build --release --no-default-features
    if ($LASTEXITCODE -ne 0) { throw "lan-mouse build failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }
$lanMouseExe = Join-Path $RepoRoot 'target\release\lan-mouse.exe'
if (-not (Test-Path $lanMouseExe)) { throw "lan-mouse.exe not found at $lanMouseExe" }
Write-OK ("lan-mouse.exe: " + [math]::Round((Get-Item $lanMouseExe).Length / 1MB, 2) + " MB")

# ---------- 4. 编 clipsync ----------
Write-H "[4/5] building clipsync (release)"
$clipsyncDir = Join-Path $ScriptDir 'clipsync'
Push-Location $clipsyncDir
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "clipsync build failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }
$clipsyncExe = Join-Path $clipsyncDir 'target\release\clipsync.exe'
if (-not (Test-Path $clipsyncExe)) { throw "clipsync.exe not found at $clipsyncExe" }
Write-OK ("clipsync.exe: " + [math]::Round((Get-Item $clipsyncExe).Length / 1MB, 2) + " MB")

# ---------- 4.5. 交叉编 aarch64-linux-musl（可选，需要 zig + cargo-zigbuild）----------
Write-H "[4.5] cross-compile clipsync for aarch64-unknown-linux-musl (optional)"
$clipsyncAarch64 = $null
if ((Get-Command zig -ErrorAction SilentlyContinue) -and (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)) {
    Push-Location $clipsyncDir
    try {
        & cargo zigbuild --release --target aarch64-unknown-linux-musl
        if ($LASTEXITCODE -ne 0) { throw "clipsync aarch64 cross-build failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
    $aarch64Out = Join-Path $clipsyncDir 'target\aarch64-unknown-linux-musl\release\clipsync'
    if (Test-Path $aarch64Out) {
        $clipsyncAarch64 = $aarch64Out
        Write-OK ("clipsync-linux-aarch64: " + [math]::Round((Get-Item $aarch64Out).Length / 1MB, 2) + " MB")
    } else {
        Write-Wm "aarch64 交叉编译完成但产物不存在于 $aarch64Out"
    }
} else {
    Write-Wm "未找到 zig/cargo-zigbuild，跳过 aarch64 交叉编译；connect.ps1 将复用远端已有 clipsync 或已暂存的 bin\clipsync-linux-aarch64"
}

# ---------- 5. 部署 ----------
Write-H "[5/5] deploy"
if ($NoDeploy) {
    Write-Wm "-NoDeploy specified, skipping deploy. Binaries:"
    Write-Host "  $lanMouseExe"
    Write-Host "  $clipsyncExe"
    return
}

$bin     = Join-Path $DeployDir 'bin'
$patches = Join-Path $DeployDir 'patches'
foreach ($d in @($DeployDir, $bin, $patches)) {
    if (-not (Test-Path $d)) { New-Item -ItemType Directory $d -Force | Out-Null }
}

# 停现有 daemon 防文件占用
Get-Process lan-mouse, clipsync -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

Copy-Item $lanMouseExe $bin -Force
Copy-Item $clipsyncExe $bin -Force
if ($clipsyncAarch64 -and (Test-Path $clipsyncAarch64)) {
    Copy-Item $clipsyncAarch64 (Join-Path $bin 'clipsync-linux-aarch64') -Force
    Write-OK "clipsync-linux-aarch64 deployed"
}
Copy-Item (Join-Path $ScriptDir 'connect.ps1') $DeployDir -Force
Copy-Item (Join-Path $ScriptDir 'USAGE.md')    $DeployDir -Force

# patches 归档：以后想给 UOS 端做完整重装时用
Copy-Item (Join-Path $RepoRoot 'input-emulation\src\uinput.rs')  $patches -Force
Copy-Item (Join-Path $RepoRoot 'input-emulation\src\lib.rs')     $patches -Force
Copy-Item (Join-Path $RepoRoot 'input-emulation\src\error.rs')   $patches -Force
Copy-Item (Join-Path $RepoRoot 'input-emulation\Cargo.toml')     (Join-Path $patches 'input-emulation-Cargo.toml') -Force
Copy-Item (Join-Path $RepoRoot 'Cargo.toml')                     (Join-Path $patches 'root-Cargo.toml') -Force
Copy-Item (Join-Path $RepoRoot 'input-capture\src\windows.rs')   (Join-Path $patches 'windows-capture.rs') -Force

Write-Host ""
Write-Host "  ====================  DONE  ====================" -ForegroundColor Green
Write-Host ("  binaries  : {0}" -f $bin)
Write-Host ("  scripts   : {0}\connect.ps1" -f $DeployDir)
Write-Host ("  docs      : {0}\USAGE.md" -f $DeployDir)
Write-Host ("  patches/  : {0}  (UOS 端首次安装会自动 scp 上去)" -f $patches)
Write-Host ""
Write-Host "  Next step:" -ForegroundColor Cyan
Write-Host ("    cd {0}" -f $DeployDir)
Write-Host  "    .\connect.ps1            # 首次跑会进交互向导（SSH 目标 / 方向 / 主机 IP / 端口）"
Write-Host ""
