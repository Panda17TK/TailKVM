# TailKVM — desktop app (Tauri + TypeScript)

This directory is the [Tauri v2](https://tauri.app/) desktop app for **TailKVM**.
For what TailKVM is, install, usage, architecture and the license, see the
**[project README](../../README.md)**.

## Layout

- `src/` — TypeScript UI (Quick Start console, monitor map, status panels)
- `src-tauri/` — Rust backend (IPC commands, capture loop, system tray)

## Develop

```bash
npm install
npm run tauri dev      # hot-reload UI + Rust backend
npm run tauri build    # production installer (target/release/bundle/)
```

> Build with `npm run tauri build`, not a bare `cargo build --release`: the latter
> bakes in the dev server URL and the packaged UI fails to load.

## Recommended IDE setup

- [VS Code](https://code.visualstudio.com/) + the
  [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode)
  and [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
  extensions.
