# TASK_LOG

TailKVM ソフトウェア KVM 開発の作業ログ（PDCA）。
作業ブランチ: `claude/pdca-tailkvm-software-kvm`

---

## Task 9B-1: WH_KEYBOARD_LL manual keyboard capture — Validation

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Check / 静的検証 (ビルド・型・lint)

### 対象実装

WH_KEYBOARD_LL を使った手動キーボードキャプチャ。controller 側で
ローカルキーボード入力をフックして抑制し、receiver 側へ転送する。

| レイヤ | ファイル | 役割 |
| --- | --- | --- |
| Win32 hook | `crates/tailkvm-win32/src/keyboard_hook.rs` | `WH_KEYBOARD_LL` グローバルフック。専用スレッドでメッセージループを回し、KeyDown/KeyUp を `KeyboardHookEvent` として mpsc 送信。フック中はローカル入力を抑制 (`return 1`)。`Ctrl+Alt+Pause` で Failsafe イベント発行。 |
| Win32 inject | `crates/tailkvm-win32/src/keyboard.rs` | `SendInput` でキーイベント / Unicode テキストを注入 (`send_key_event` / `send_keyboard_text`)。 |
| protocol | `crates/tailkvm-net/src/protocol.rs` | `WireMessage::KeyboardText` / `WireMessage::KeyboardKey` を追加。 |
| tauri cmd | `apps/tailkvm-ui/src-tauri/src/lib.rs` | `start/stop_keyboard_hook_capture`、`send_test_keyboard_text`、`send_test_key_tap`。controller 側でフックイベントを WireMessage へ転送、receiver 側で `KeyboardText`/`KeyboardKey` を注入。 |
| UI | `apps/tailkvm-ui/src/main.ts` + `index.html`(動的生成) | キーボードテキスト送信 / 単発キー / フックキャプチャ開始停止のボタン配線。 |

### 実装レビューで確認した設計上のポイント

- **ローカル抑制**: `low_level_keyboard_proc` がイベント送信成功時に `1` を返し、
  ローカルへ伝播させない。フック未起動時 (`send_event` が false) は `CallNextHookEx` でパススルー。
- **Failsafe**: `Ctrl+Alt+Pause` (VK_PAUSE + Ctrl + Alt) でキーボード/マウス両キャプチャと
  remote_control を停止。controller が制御不能になった場合の脱出口。
- **スタックキー解放**: フック停止時に `pressed_keys` に残っている押下中キーへ
  KeyUp を送信し、receiver 側でキーが押しっぱなしになるのを防止。
- **二重起動防止**: `keyboard_hook_running.swap(true, …)` と
  hook 側 `EVENT_SENDER` の `guard.is_some()` で多重起動を防ぐ。
- **scan code 優先注入**: `send_key_event` は scan_code があれば `KEYEVENTF_SCANCODE`、
  なければ vk を使用。extended key フラグも伝播。

### この検証で行った修正

- `apps/tailkvm-ui/src/main.ts`: フォーム要素を生成する `app.innerHTML` テンプレートに、
  キーボード関連 UI 要素を追加した。
  既存の `main.ts` はイベント配線で以下の要素を `querySelector(...)!` 参照していたが、
  テンプレート側に該当要素が無く、起動時に `addEventListener` が null で例外を投げ
  **UI 全体が初期化に失敗する**状態だった。追加した要素:
  `#keyboard-text`, `#send-keyboard-text`, `#send-key-enter`, `#send-key-backspace`,
  `#send-key-tab`, `#send-key-escape`, `#start-keyboard-hook-capture`,
  `#stop-keyboard-hook-capture`。

### 静的検証結果

| チェック | コマンド | 結果 |
| --- | --- | --- |
| フォーマット | `cargo fmt --all` | ✅ exit 0 |
| 型/ビルド (Rust) | `cargo check --workspace` | ✅ exit 0 (warning 1 件、後述) |
| lint | `cargo clippy -p tailkvm-win32` | ✅ exit 0 (同 warning 1 件) |
| UI ビルド | `npm run build` (tsc + vite) | ✅ exit 0、6 modules、エラーなし |

### 既知の warning（非ブロッキング・フォローアップ候補）

- `clashing_extern_declarations`: `crates/tailkvm-win32/src/keyboard.rs:33` と
  `mouse.rs:44` が同名 `SendInput` をそれぞれローカルの `Input` 型で `extern` 宣言している。
  両 `Input` 構造体のメモリレイアウトは同一 (Win32 `INPUT`) のため実害はないが、
  コンパイラ警告が出る。
  - フォローアップ案: `windows-sys` の `Win32_UI_Input_KeyboardAndMouse`
    （既に Cargo.toml で有効化済み）が提供する `SendInput`/`INPUT` を共用するか、
    共通モジュールに単一の extern 宣言を置く。Task 9B-1 検証の範囲外として今回は未対応。

### 実機検証手順（Bob-note 実機が必要 — 未実施）

ホスト 2 台（controller = 操作元 / receiver = 操作先、例: Bob-note）が Tailscale 上で
接続され、TCP セッションが確立している前提。フック登録には**管理者権限**が必要な場合がある。

1. **テキスト注入**
   - controller の UI で receiver へ接続 (`Connect peer` → TCP connected)。
   - receiver 側でメモ帳等のテキスト入力欄にフォーカス。
   - controller の `Keyboard text` に文字列を入力し `Send keyboard text`。
   - 期待: receiver 側に同じ文字列（UTF-16 Unicode 注入）が入力される。
   - 確認: 日本語/絵文字などサロゲートペアを含む文字列も正しく入る。

2. **単発キータップ**
   - receiver 側テキスト欄にフォーカス。
   - controller で `Test Enter` / `Test Backspace` / `Test Tab` / `Test Escape` を押下。
   - 期待: receiver 側で各キーが 1 回ずつ作用（改行 / 1 文字削除 / タブ移動 / Esc）。
   - 確認: 押しっぱなしにならない（down→25ms→up）。

3. **フックキャプチャ（実キーボード転送）**
   - controller で `Capture keyboard` を押下。
   - 期待: controller 側のローカルキーボード入力が**抑制**され、
     receiver 側に転送される。controller のローカルアプリには文字が入らないこと。
   - controller で通常の文字キー / 矢印キー / 修飾キー (Shift+文字, Ctrl+C 等) を入力。
   - 確認: receiver 側で修飾キー込みで正しく再現される。extended key（矢印/Delete 等）も確認。
   - 確認: UI の TCP state(`last_event`) に `Keyboard hook event forwarded ...` が更新される。

4. **Failsafe**
   - フックキャプチャ中に `Ctrl + Alt + Pause` を押下。
   - 期待: キーボード/マウス両キャプチャと remote control が即時停止。
     `last_event` に `Keyboard failsafe ... received` が表示される。
   - 確認: 停止後 controller のローカル入力が通常どおり戻る。

5. **スタックキー解放**
   - 何かキーを押下したまま `Stop keyboard capture`（または Failsafe）。
   - 期待: receiver 側でそのキーが押しっぱなしにならない（KeyUp が補完送信される）。

6. **多重起動防止 / 接続前ガード**
   - 未接続状態で `Capture keyboard` → エラー文言が UI に出ること。
   - 連続で `Capture keyboard` → 二重起動せず `already running` メッセージ。

> 上記 1〜6 は実機 2 台が必要なため本検証（静的）では未実施。実機検証時に結果を追記する。

### 結論

- 静的検証（fmt / check / clippy / UI build）はすべて成功。
- main.ts のテンプレート欠落バグを修正し、UI が起動できる状態にした。
- 残課題: `SendInput` 二重宣言 warning（非ブロッキング）、実機 2 台での機能検証（手順を上記に記載）。
