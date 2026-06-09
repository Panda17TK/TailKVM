# TailKVM

**One mouse, keyboard and clipboard across your Windows PCs — over [Tailscale](https://tailscale.com/).**

[![CI](https://github.com/Panda17TK/TailKVM/actions/workflows/ci.yml/badge.svg)](https://github.com/Panda17TK/TailKVM/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Panda17TK/TailKVM?include_prereleases&sort=semver)](https://github.com/Panda17TK/TailKVM/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows 11](https://img.shields.io/badge/platform-Windows%2011-0078D6)](#requirements)

TailKVM is a **software KVM switch**: drive a second Windows PC with the mouse and
keyboard of the one in front of you, and share the clipboard between them — with no
extra hardware. Move the cursor off a screen edge and it crosses to the remote
machine; move it back and you return to your own. All traffic rides your private
**tailnet**, so the two machines only need to be reachable over Tailscale.

> 日本語クイックスタートは「[Quick Start](#quick-start)」を参照。UI は日本語/英語併記です。

---

## Features

- **Seamless edge crossing** — flick the cursor to a configured screen edge to take
  over the remote PC; flick back to return. No hotkey juggling.
- **Multi-monitor aware** — per-monitor edge detection, Per-Monitor-V2 DPI, mixed
  resolutions, and Windows virtual-screen coordinates. Park the remote at a corner
  to cross on **both** a vertical and a horizontal edge.
- **Keyboard parity** — JIS/US layouts, IME / 全角・半角, Win and Alt+Tab, with
  stuck-key release on disconnect.
- **Clipboard sharing** between controller and receiver.
- **Pointer-speed control** to compensate for resolution differences after crossing.
- **Tray-first** — runs quietly in the system tray; a guided Quick Start panel walks
  you through receive → connect → position → control.
- **Tailscale-native transport** over TCP (default port `47110`), scoped to the
  Tailscale CGNAT range (`100.64.0.0/10`).

## Requirements

- **Windows 11** (x64). TailKVM uses Win32 input/hook/monitor APIs and is Windows-only.
- **[Tailscale](https://tailscale.com/)** installed and signed in on **both** PCs,
  with the two machines on the same tailnet.

## Install

1. Download the latest installer from the
   [**Releases**](https://github.com/Panda17TK/TailKVM/releases) page
   (`TailKVM_x.y.z_x64-setup.exe`).
2. Run it on **both** PCs (the one you control from and the one you control).
3. Launch TailKVM. It appears in the system tray and opens the Quick Start panel.

> The receiver side may need a one-time firewall rule to accept inbound `47110`
> from your tailnet — the app offers an **Install firewall rule** button under
> advanced settings.

## Quick Start

The Quick Start panel is a numbered console:

1. **RX — Receive** *(on the PC you want to control)*: click **Start receiver** so it
   listens for a controller.
2. **01 — Connect** *(on the controller PC)*: enter the receiver's Tailscale IP (or
   pick it from the candidate list) and click **Connect**.
3. **02 — Position**: drag the **相手PC / peer** tile on the monitor map to where the
   remote screen sits relative to yours. The map shows which edges will cross.
4. **03 — Control**: click **Start KVM**. Move the cursor off the configured edge to
   drive the remote PC; move back to return. Adjust **pointer speed** if the remote
   feels slow.

Failsafe: **Ctrl + Alt + Pause** stops all capture immediately.

## Build from source

Prerequisites: [Rust](https://rustup.rs/) (stable), [Node.js](https://nodejs.org/)
18+, and the [Tauri prerequisites for Windows](https://tauri.app/start/prerequisites/)
(WebView2 + MSVC build tools).

```bash
git clone https://github.com/Panda17TK/TailKVM.git
cd TailKVM/apps/tailkvm-ui
npm install

# Develop (hot-reload UI)
npm run tauri dev

# Production build (installer under target/release/bundle/)
npm run tauri build
```

> **Important:** build the desktop app with **`npm run tauri build`**, not a bare
> `cargo build --release`. A plain cargo build bakes in the dev server URL
> (`localhost:1420`) and the packaged app will fail to load its UI.

## How it works

```
apps/tailkvm-ui/        Tauri v2 desktop app
  src/                  TypeScript UI (Quick Start, monitor map, status)
  src-tauri/            Rust backend: IPC commands, capture loop, tray
crates/
  tailkvm-core/         Shared core types
  tailkvm-win32/        Win32 wrappers: monitors, cursor, hooks, screen-space math
  tailkvm-net/          Wire protocol + Tailscale TCP transport
```

The controller integrates raw mouse deltas into a **combined screen space** that
maps your local monitors and the remote screen. When the cursor reaches a crossing
edge, the local cursor is parked and confined, and input is forwarded to the
receiver as `WireMessage`s (mouse move/button/wheel, keyboard key/text, clipboard).
The receiver injects those events via `SendInput`. Heartbeats keep the session
alive; a single-slot "newest-wins" receiver replaces stale sessions.

See [`docs/`](docs/) for design notes (keyboard/IME, raw input, multi-client
runtime, OS limitations).

## Security

TailKVM forwards real mouse and keyboard input between machines. **Only connect
machines you own and trust, on your own tailnet.** Transport is scoped to the
Tailscale CGNAT range. See [SECURITY.md](SECURITY.md) for the threat model and how
to report vulnerabilities.

## Limitations

- Windows 11 only (no macOS/Linux).
- Some UAC-elevated / secure-desktop surfaces cannot receive injected input.
- Best with both machines on a stable tailnet; brief drops auto-reconnect.

## Contributing

Issues and PRs are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for setup, code
style (`cargo fmt` / `clippy` / `tsc`), and the PR process.

## License

[MIT](LICENSE) © 2026 Taiki Handa.
