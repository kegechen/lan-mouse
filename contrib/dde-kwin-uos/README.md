# DDE-KWin UOS Deployment Bundle

End-to-end Windows ↔ UOS (deepin Wayland, dde-kwin 5.15) KVM deployment.
This directory ships everything you need to redeploy after a fresh checkout.

Companion code paths in this repo:
- `input-emulation/src/uinput.rs`        — uinput emulation backend
- `input-emulation/src/lib.rs`           — Backend::Uinput dispatch
- `input-capture/src/windows.rs`         — sticky-corners + V-arrow indicator

Tooling here:
- `connect.ps1`     — one-shot orchestrator (probe → install → start → activate)
- `USAGE.md`        — full operator manual + caveats (Wayland clipboard limit, etc.)
- `clipsync/`       — bidirectional text clipboard sync daemon (cross-platform Rust)

See `USAGE.md` for the day-to-day workflow.
