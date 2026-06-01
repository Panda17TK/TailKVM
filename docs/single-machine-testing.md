# TailKVM 単体マシン動作テスト方法論

TailKVM は本来 2 台（controller = 操作元 / receiver = Bob-note）で動かす Software KVM だが、
**この端末 1 台でも検証可能な範囲が大きい**。本書はその方法論を 3 層に整理する。

| 層 | 何を検証するか | 自動化 | コマンド / 手順 |
| --- | --- | --- | --- |
| L1 トランスポート | wire 形式 + TCP 改行フレーミングの往復（controller↔receiver の互換） | 完全自動・CI 可 | `cargo test -p tailkvm-net` |
| L2 注入 FFI | Win32 クリップボード get/set の実 FFI（この端末で実動作） | 自動・要 Windows デスクトップ | `cargo test -p tailkvm-win32 --test clipboard_roundtrip -- --ignored` |
| L3 GUI ループバック | 実アプリで TCP 接続 + キー/マウス/クリップボード注入 | 手動 | 本書 §3 |

`cargo test --workspace` で L1 を含む全自動テスト（26 件）が走る。L2 は実クリップボードを汚すため
`#[ignore]`（明示実行）。

## 1. L1 — ループバック・トランスポートテスト（自動）

`crates/tailkvm-net/tests/loopback.rs`。`127.0.0.1` 上で writer(controller) と reader(receiver) を繋ぎ、
全 `WireMessage` を `encode_line` で送出 → receiver と同じ `BufReader::lines()` + `decode_line` で
受信し、正準 JSON で一致を確認する。注入は行わないのでどこでも安全。

- `loopback_preserves_all_messages_written_individually`: 1 メッセージずつ送って順序・内容一致。
- `loopback_framing_survives_coalesced_write`: 全メッセージを 1 回の write に連結 → TCP coalescing 下でも
  改行フレーミングが正しく分割されること（フレーミング回帰の検出）。

**意義**: controller が書くものを receiver が正しく読めること（実 TCP + 実フレーミング）を 1 台で保証。

## 2. L2 — クリップボード実 FFI テスト（自動・要 Windows）

`crates/tailkvm-win32/tests/clipboard_roundtrip.rs`。`set_clipboard_text` → `get_clipboard_text` で
Unicode + 絵文字を含む文字列が完全往復することを、**実 Windows クリップボード**で確認する。

```powershell
cargo test -p tailkvm-win32 --test clipboard_roundtrip -- --ignored
```

- 実クリップボードを上書きするため `#[ignore]`（通常の `cargo test` では走らない）。
- 他プロセスがクリップボードを掴んでいると `OpenClipboard` が失敗しうる（稀）。再実行で解消。

**意義**: Task 11 で書いた CF_UNICODETEXT FFI が実機で正しく動くことを 1 台で実証済み（本セッションで pass 確認）。

## 3. L3 — GUI ループバック・スモークテスト（手動・1 台）

実アプリを 1 台で起動し、自分自身へ TCP 接続して注入系を確認する。

### 前提・重要な仕様

- `start_mouse_capture`（remote mode のマウス移動キャプチャ）は **`127.*`/localhost を拒否**する
  （マウスフィードバックループ防止）。したがって **マウス移動キャプチャと画面端切替は 1 台では検証不可**（要 2 台）。
- 一方、**個別送信コマンドと receiver 注入は localhost でも動作する**:
  `send_test_keyboard_text` / `send_test_key_tap` / `send_test_mouse_click` /
  `send_test_mouse_double_click` / `send_test_mouse_move` / `send_clipboard_text`。
- WH_*_LL フックのローカル抑止（`Capture keyboard` / `Capture mouse`）は 1 台で使うと
  **自分の入力が抑止される**ので注意（停止は Ctrl+Alt+Pause / Stop ボタン）。スモークでは使わなくてよい。

### 手順

1. 開発起動: `cd apps/tailkvm-ui && npm run tauri dev`（またはインストール済み TailKVM を起動）。
2. アプリで **Start receiver**（既定ポート 47110）。`Receiver listening on 0.0.0.0:47110` を確認。
3. 同じアプリで **Connect peer** に `127.0.0.1` を入力して接続。
   - 期待: `TCP connected` → Hello/HelloAck → `last_event` に keyboard layout 交換のログ。
   - 同一マシンなのでレイアウト不一致警告は出ない（locale/keyboard_type 一致）。
4. **キーボードテキスト注入**: メモ帳を開いてフォーカス → アプリの `Keyboard text` に文字列を入れ
   `Send keyboard text`。
   - 期待: メモ帳にその文字列（日本語/絵文字含む）が入力される。
5. **単発キー**: メモ帳にフォーカス → `Test Enter` / `Test Backspace` / `Test Tab` / `Test Escape`。
   - 期待: 各キーが 1 回ずつ作用（改行/1 文字削除/タブ/Esc）。押しっぱなしにならない。
6. **マウスクリック/ホイール**: 任意のウィンドウへ `send_test_mouse_click` 等。
   - 期待: クリック/ホイールが受信側（=この端末）に作用。
7. **クリップボード共有（Task 11）**: 何かテキストをコピー → `Send clipboard to peer` →
   メモ帳で Ctrl+V。
   - 期待: コピーした内容が貼り付く。
   - 同一内容で再度 `Send clipboard to peer` → `last_event` が `Clipboard unchanged...` でスキップ（echo guard）。
   - 空クリップボード/非テキスト時はエラー文言。

### 1 台では検証できない（要 Bob-note 2 台）

- マウス移動キャプチャ・画面端切替・remote mode 進入/復帰（localhost ガード + フィードバック）。
- WH_*_LL のローカル**抑止**の実挙動（1 台だと自分が操作不能になり評価しづらい）。
- Tailscale 越し TCP 疎通・Firewall rule（`100.64.0.0/10`）。
- **T5 stuck-key 解放**: controller がキー押下中に切断 → receiver で解放、は 2 台が確実
  （1 台ではフック抑止と注入が同一マシンで干渉するため）。
- これら 2 台手順は `TASK_LOG.md` の各タスクエントリに記載。

## 4. 推奨ルーチン

- コミット前/CI: `cargo fmt --all && cargo check --workspace && cargo test --workspace && (cd apps/tailkvm-ui && npm run build)`。
- この端末での機能確認: 上記に加え L2（`--ignored`）と L3 スモークを随時。
- 配布前: `npm run tauri build` でインストーラ生成（§ TASK_LOG Cycle 10）。
