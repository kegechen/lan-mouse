# DDE-KWin UOS Deployment Bundle

End-to-end Windows ↔ UOS (deepin Wayland, dde-kwin 5.15) KVM deployment.
This directory ships everything you need to redeploy after a fresh checkout.

Companion code paths in this repo:
- `input-emulation/src/uinput.rs`        — uinput emulation backend
- `input-emulation/src/lib.rs`           — Backend::Uinput dispatch
- `input-capture/src/windows.rs`         — sticky-corners + V-arrow indicator

Tooling here:
- `build.ps1`       — **start here on a fresh checkout**: builds both binaries
                      and deploys them to D:\tools\lan-mouse (or -DeployDir)
- `connect.ps1`     — one-shot orchestrator (probe → install → start → activate)
- `USAGE.md`        — full operator manual + caveats (Wayland clipboard limit, etc.)
- `clipsync/`       — bidirectional text clipboard sync daemon (cross-platform Rust)

## Quickstart from a fresh clone

```powershell
git clone -b feature/dde-kwin-uinput-stickycorners https://github.com/kegechen/lan-mouse.git
cd lan-mouse\contrib\dde-kwin-uos
.\build.ps1                       # auto-installs Rust if missing, builds + deploys
cd D:\tools\lan-mouse
.\connect.ps1                     # interactive setup wizard, then daily use
```

`build.ps1` flags: `-DeployDir`, `-Proxy`, `-SkipProxy`, `-NoDeploy`.

See `USAGE.md` for the day-to-day workflow + Wayland clipboard caveat.
