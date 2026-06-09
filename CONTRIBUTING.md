# Contributing to TailKVM

Thanks for your interest! TailKVM is a Windows-only Tauri + Rust application.

## Prerequisites

- **Windows 11** (the app links Win32 APIs; it does not build on macOS/Linux)
- [Rust](https://rustup.rs/) (stable toolchain) with `rustfmt` and `clippy`
- [Node.js](https://nodejs.org/) 18+
- [Tauri Windows prerequisites](https://tauri.app/start/prerequisites/)
  (WebView2 runtime + MSVC build tools)
- [Tailscale](https://tailscale.com/) on two machines to test the live KVM flow

## Getting started

```bash
git clone https://github.com/Panda17TK/TailKVM.git
cd TailKVM/apps/tailkvm-ui
npm install
npm run tauri dev      # hot-reload UI + Rust backend
```

> Build releases with **`npm run tauri build`** (never a bare
> `cargo build --release` — it bakes in the dev URL and the packaged UI won't load).

## Checks before opening a PR

Run all of these from the repo root and make sure they pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Frontend (from `apps/tailkvm-ui/`):

```bash
npx tsc --noEmit
```

CI runs the same checks on `windows-latest`.

## Code style

- **Rust:** `rustfmt` defaults, `clippy` clean (warnings are errors). Prefer small,
  focused modules; handle errors explicitly; document `unsafe` with a `// SAFETY:`
  comment.
- **TypeScript:** keep the UI typed (`tsc --noEmit` must pass), avoid `any`, and keep
  DOM element IDs stable (the Rust IPC layer and tests reference them).
- Keep commits scoped and write a clear message (`feat:`, `fix:`, `refactor:`, …).

## Testing the live KVM

Much of the value (seamless crossing, keyboard, clipboard) can only be verified with
**two real machines on a tailnet**. When a change touches the capture/cross path,
please describe how you tested it across two PCs. See [`docs/`](docs/) for design
notes and [`docs/single-machine-testing.md`](docs/single-machine-testing.md) for what
can be checked on one machine.

## Reporting issues

Open a GitHub issue with your Windows version, monitor layout (counts/resolutions),
and steps to reproduce. For security-sensitive reports, see
[SECURITY.md](SECURITY.md) instead.
