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
- 残課題: `SendInput` 二重宣言 warning（非ブロッキング、Task 9A.5 で対応）、実機 2 台での機能検証（手順を上記に記載）。

---

## Task 9B-2: remote mode active 時に keyboard capture を自動 ON/OFF

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Do / 実装

### 目的

remote mode（マウスがスイッチエッジを越えてリモート操作中）に入ったら、
キーボードキャプチャも自動的に開始し、リモートから戻る/停止したら自動的に停止する。
これまでキーボードキャプチャは `start/stop_keyboard_hook_capture` の手動操作のみだった。

### 実装方針

既存の「remote 時にマウスフックを自動 ON/OFF する」ロジック
（`start_mouse_capture` コマンドが spawn する非同期キャプチャループ）に対称的に組み込んだ。
キーボード専用の独立トリガは追加せず、マウス remote 状態に追従させることで状態の一貫性を保つ。

| 箇所 | 変更 |
| --- | --- |
| `start_mouse_capture` の spawn 前 | `keyboard_hook_running` / `keyboard_hook` を closure へ clone。 |
| remote 有効化時（エッジ通過 → `start_mouse_hook_forwarding` の直後） | `start_keyboard_hook_forwarding(..., "auto")` を追加。失敗しても last_event に記録して続行。 |
| キャプチャループ終了時クリーンアップ | `stop_mouse_hook_forwarding` の直後に `stop_keyboard_hook_forwarding(..., "auto")` を追加。 |
| `stop_mouse_capture` コマンド | 即時停止の対称性のため `stop_keyboard_hook_forwarding(..., "auto")` を追加。 |

### 状態遷移カバレッジ

remote の解除は「return edge 到達 → `capture_running=false`」「手動 stop」「Ctrl+Alt+Pause failsafe」の
いずれもキャプチャループの終了に集約されるため、ループ終了時クリーンアップに停止を入れることで
全経路をカバーできる。`keyboard_hook_running.swap(true)` ガードにより、
手動キャプチャと auto キャプチャが重なっても二重起動しない。

- **Ctrl+Alt+Pause failsafe は削除していない**（マウスループ側のチェックとキーボードフック側の
  Failsafe イベントの両方を維持）。

### 静的検証結果

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0（既知の `SendInput` warning のみ） |
| `npm run build` | ✅ exit 0 |

### 実機検証手順（Bob-note 実機が必要 — 未実施）

1. controller で remote mode を ON にしてマウスキャプチャ開始 → スイッチエッジへカーソル移動。
   - 期待: remote 有効化と同時に `last_event` に `Auto keyboard capture failed` が出ない、かつ
     controller のローカルキーボード入力が抑制され receiver 側へ転送される。
2. リモート操作中にキー入力 → receiver 側に反映されること。
3. return edge へ戻る → キーボードキャプチャも自動停止し、ローカル入力が戻ること。
4. `Stop capture` / Ctrl+Alt+Pause → マウス・キーボード両方が停止すること。
5. 手動 `Capture keyboard` 実行中に remote へ入っても二重起動エラーにならないこと。

### 結論

- remote mode 連動のキーボード自動 ON/OFF を実装。静的検証はすべて成功。
- 残課題: 実機 2 台での連動動作確認（手順を上記に記載）。

---

## Task 9A.5: SendInput FFI 共通化

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Do / リファクタ（警告解消）

### 目的

Task 9B-1 検証で検出した `clashing_extern_declarations` warning の解消。
`mouse.rs` と `keyboard.rs` が同名 `SendInput` をそれぞれローカルの `Input` 型で
`extern` 宣言していたため、同一クレート内で同名・異シグネチャの extern 宣言が衝突していた。

### 実装

新規モジュール `crates/tailkvm-win32/src/input.rs` を作成し、`SendInput` の FFI を一元化。

- `Input`（Win32 `INPUT`）と `InputUnion`（`MOUSEINPUT`/`KEYBDINPUT` のタグ付き union）、
  `MouseInput` / `KeyboardInput`、`INPUT_MOUSE` / `INPUT_KEYBOARD` を `pub` で定義。
- `SendInput` の `extern` 宣言はこのモジュールに **1 箇所だけ** 置き、
  薄いラッパ `send_input(&Input) -> u32` を公開（挿入されたイベント数を返す）。
- `mouse.rs`: 独自の `Input`/`InputUnion`/`MouseInput`/`SendInput`/`INPUT_MOUSE` 定義を削除し、
  `crate::input` から import。`send_mouse_input` は `send_input(&input)` を使用。
- `keyboard.rs`: 同様に独自定義を削除し `crate::input` を使用。
- `lib.rs`: `pub mod input;` を追加。

union のメモリレイアウトは従来と同一（x64 で `INPUT` = 40 bytes）。`#[repr(C)]` 維持。

### 静的検証結果

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、**warning ゼロ**（`clashing_extern_declarations` 解消を確認） |
| `cargo clippy --workspace` | `tailkvm-win32` は警告ゼロ。`tailkvm-ui` に既存の clippy スタイル lint（`too_many_arguments` 9/7、`manual_is_multiple_of` 等）が残るが本タスク対象外。 |
| `npm run build` | ✅ exit 0 |

> 補足: `start_keyboard_hook_forwarding` の引数が 9 個で `too_many_arguments` 警告が出る。
> 機能には影響しないが、将来 `AppState` 由来のフック関連フィールドを構造体にまとめると解消できる
> （フォローアップ候補）。

### 結論

- `SendInput` FFI を `input.rs` に共通化し、`clashing_extern_declarations` warning を解消。
- 実機での挙動はレイアウト不変のため従来どおり（追加の実機検証手順なし）。

---

## Task 9C: keyboard layout foundation

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Do / 基盤実装

### 目的

JIS/US などレイアウト差分処理（Task 9D 設計、将来の remap）に向けた基盤として、
**アクティブなキーボードレイアウトと物理キーボード種別を識別**する仕組みを用意する。
本タスクは「識別」までで、実際の remap / IME 処理は含めない。

### 実装

新規モジュール `crates/tailkvm-win32/src/keyboard_layout.rs`。
独立した 2 つの軸を取得する:

| 軸 | API | 意味 |
| --- | --- | --- |
| 入力ロケール (HKL) | `GetKeyboardLayout(foreground thread)` | OS がスキャンコード→文字へ写像するソフトレイアウト。low word が言語 ID（日本語 `0x0411`）。 |
| 物理キーボード種別 | `GetKeyboardType(0/1/2)` | ハードの種別。`7` = 日本語(JIS)キーボード。変換/無変換・¥・JIS 括弧位置など物理キーの有無を決める。 |

- `KeyboardLayoutInfo`（`Serialize`）: hkl, language_id, primary_language,
  is_japanese_locale, keyboard_type, keyboard_subtype, function_keys,
  is_jis_keyboard, label。
- `current_keyboard_layout()`: foreground window のスレッドの HKL を読む
  （foreground window が無ければ calling thread の `GetKeyboardLayout(0)` にフォールバック）。
- IME 変換モード（半角/全角・かな/ローマ字・変換 ON/OFF）は HKL に含まれないため
  **意図的にスコープ外**（Task 9D で設計）。
- Tauri command `get_keyboard_layout` を追加し、UI に "Keyboard Layout" カード
  （`#refresh-keyboard-layout` ボタン + `#keyboard-layout-summary`）を追加して
  `info.label` を表示。
- `lib.rs` に `pub mod keyboard_layout;` 追加。windows-sys の features は
  既存の `Win32_UI_Input_KeyboardAndMouse` / `Win32_UI_WindowsAndMessaging` で充足
  （Cargo.toml 変更なし）。

### 静的検証結果

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0 |

### 実機検証手順（Bob-note 実機 / 各ホストで確認）

1. 日本語 IME + JIS キーボードの Windows で `Check keyboard layout` を押下。
   - 期待: `locale=0x0411 (Japanese), keyboard_type=7 (JIS)` のような表示。
2. US レイアウト + US キーボードのホストで押下。
   - 期待: `locale=0x0409, keyboard_type=4`（101/102 拡張）のような表示。
3. controller / receiver 双方で取得し、レイアウト差分の有無を確認できること
   （Task 9D の remap 設計のための実測データ収集に使用）。

### 結論

- レイアウト/物理キーボード識別の基盤を実装。静的検証はすべて成功。
- 残課題: 各ホストでの実測表示確認（手順を上記に記載）、Task 9D の remap/IME 設計へ接続。
