# TASK_LOG

TailKVM ソフトウェア KVM 開発の作業ログ（PDCA）。
作業ブランチ: `claude/pdca-tailkvm-software-kvm`

---

## Current Code Analysis (2026-06-02 セッション開始時)

サブエージェント実行は本環境の巨大な base context（数百の MCP ツール/skill 注入）により
"Prompt is too long" で失敗したため、main Claude が codebase-analyst / safety-reviewer /
input-hook-specialist の 3 レンズを直接適用して分析した（最終統合判断は main のみ、の方針に合致）。

### アーキテクチャ

| レイヤ | 場所 | 役割 |
| --- | --- | --- |
| orchestrator | `apps/tailkvm-ui/src-tauri/src/lib.rs` (2166行) | Tauri commands、TCP controller/receiver、マウスキャプチャループ、remote mode 状態遷移、hook 起動停止。 |
| win32 input | `crates/tailkvm-win32/src/{mouse,keyboard,input,cursor}.rs` | `SendInput` 共通化 (`input.rs`)、相対マウス/ボタン/ホイール、キー/Unicode 注入、`GetCursorPos`/`SetCursorPos`、failsafe 判定。 |
| win32 hook | `keyboard_hook.rs` / `mouse_hook.rs` | `WH_KEYBOARD_LL`/`WH_MOUSE_LL` グローバルフック。専用スレッド + PeekMessage ループ、mpsc でイベント送信、成功時 `return 1` でローカル抑制。 |
| win32 その他 | `monitor.rs`（仮想スクリーン/DPI/トポロジ）、`keyboard_layout.rs`（HKL/JIS 識別 + `mismatch_with`）、`firewall.rs`（昇格 PowerShell で New-NetFirewallRule）。 |
| protocol | `crates/tailkvm-net/src/protocol.rs` | `WireMessage` enum（serde tag="type", snake_case）+ `encode_line`/`decode_line`（改行フレーミング）。 |

- **役割決定**: `start_tcp_receiver`→receiver、`connect_tcp_peer`→controller（動的）。
- **AppState**: atomics（`capture_running`/`mouse_hook_running`/`keyboard_hook_running`/`receiver_running`）、
  `RemoteControlState`、hook ハンドル、controller チャネル。
- **remote mode 遷移**: mouse capture loop (`lib.rs:881`) がエッジ検出→`MouseSetPosition`→
  mouse+keyboard hook 自動起動→cursor lock warp→return edge でローカル復帰 + 全 hook 解体。

### 安全性レビュー所見

- ✅ **Failsafe は二重化**: (a) keyboard hook proc が `VK_PAUSE+Ctrl+Alt` を検出し `Failsafe` イベント送信
  (`keyboard_hook.rs:170`)、(b) mouse loop が毎 interval `is_ctrl_alt_pause_pressed()` をポーリング
  (`lib.rs:908`)。keyboard hook 未起動でも (b) が効く。
- ✅ **Firewall** の RemoteAddress 既定が `100.64.0.0/10`（Tailscale CGNAT）でスコープ済み（`firewall.rs:21`）。
- ✅ **controller 側 stuck key/button 解放**: ループ終了時に pressed_keys/pressed_buttons を drain して
  KeyUp/ButtonUp 送信（`lib.rs:294`, `lib.rs:511`）。
- ⚠️ **発見課題 (med)**: **receiver 側に独立した stuck key/button 解放のセーフティネットが無い**。
  TCP がキー押下中に切れると最後の KeyUp が届かず、Bob-note 側でキーが押しっぱなしになりうる。
  → 将来タスク「receiver disconnect 時に全押下解放」候補。
- ⚠️ **発見課題 (low)**: `start_keyboard_hook_forwarding` の引数 9 個（既知 `too_many_arguments`）。

### テストカバレッジ

- ワークスペース全体で**実質テストゼロ**（`tailkvm-core::add` のプレースホルダのみ）。
- ユーザ要望のテスト（protocol serialization / edge mapping / remote entry / return edge /
  mismatch 判定 / stuck key 解放）は**すべて未実装**。純粋ロジック関数が多数あり安全に追加可能:
  - `protocol.rs`: `encode_line`/`decode_line`
  - `keyboard_layout.rs`: `mismatch_with`
  - `lib.rs`: `is_cursor_at_edge` / `remote_entry_position` / `local_return_position` /
    `is_remote_return_edge` / `normalize_edge` / `key_to_test_key`（private、同ファイル内 test で可）

### 最も安全な次タスク 3 件（実機不要・fmt/check/build で検証可）

1. **protocol serialization round-trip tests**（`tailkvm-net`、純 lib、最小リスク）← Cycle 1 採用
2. **edge / remote-entry / return-edge mapping tests**（`lib.rs` の純関数）
3. **`mismatch_with` layout tests**（`keyboard_layout.rs` の純ロジック）

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

---

## Task 9D: IME / 半角全角 / JIS-US 差分の設計メモ作成

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Plan / 設計ドキュメント

### 成果物

`docs/keyboard-layout-ime-design.md`（新規）。コード変更なしの設計メモ。

### 内容の要約

- **現状実装の整理**: `KeyboardKey`(scan/vk) 経路 = 物理キー再現、`KeyboardText`(Unicode) 経路
  = 文字再現、の二系統。scan は receiver の HKL で文字へ写像される点が JIS/US 差分の根。
- **差分を 3 軸に分解**: (A) 物理キーボード(JIS/US, `GetKeyboardType`)、
  (B) 入力ロケール/ソフトレイアウト(HKL)、(C) IME 状態（HKL にも含まれない別物）。
- **各軸の問題**を表で整理（特に日本語/IME 絡みで物理経路が破綻する点）。
- **推奨アプローチ（段階導入）**:
  1. レイアウト情報交換 + 不一致警告（最小）
  2. 制御/修飾キーは物理経路、文字生成キーは `ToUnicodeEx` で controller 側解決 → Unicode 送出
  3. IME は controller 側で完結（確定文字を送る）。確定文字取得方法は要 PoC。
  4. 任意: JIS↔US 物理 remap テーブル。
- **関連 Win32 API**（`ToUnicodeEx`/`MapVirtualKeyEx`/`VkKeyScanEx`/`Imm*`）と
  **未解決事項 / 次アクション**を列挙。

### 静的検証結果（ドキュメントのみ・回帰確認）

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0 |

### 結論

- 設計メモを作成し、フェーズ 1（レイアウト情報交換 + 不一致警告）を次の実装候補として明文化。
- 残課題: 実機での `language_id`/`keyboard_type` 実測値のメモ追記、`ToUnicodeEx` PoC。

---

## PDCA セッションまとめ（2026-06-01）

本セッションで Task 9B-1 / 9B-2 / 9A.5 / 9C / 9D の 5 件を実施・push 完了。
すべて `claude/pdca-tailkvm-software-kvm` ブランチへコミット（main への push / force push なし）。

| Task | 種別 | 主成果 | コミット |
| --- | --- | --- | --- |
| 9B-1 | Check | 手動 keyboard capture 検証、UI テンプレ欠落バグ修正 | `0ca7cb2` |
| 9B-2 | Do | remote mode 連動の keyboard 自動 ON/OFF | `cfe78c2` |
| 9A.5 | Do | `SendInput` FFI 共通化（warning 解消） | `1de926d` |
| 9C | Do | keyboard layout 識別基盤 + UI 表示 | `51c030c` |
| 9D | Plan | IME/JIS-US 差分 設計メモ | （本コミット） |

共通の残課題: 実機 2 台（Bob-note 含む）での機能・連動・レイアウト実測検証
（各タスクに手動手順を記載済み）。

---

## Task 9D フェーズ1: レイアウト情報交換 + 不一致警告

- 日付: 2026-06-01
- 担当: Claude (Opus 4.8)
- 種別: Do / 実装（設計メモ `docs/keyboard-layout-ime-design.md` フェーズ1 の実装）

### 目的

接続時に controller / receiver が互いのキーボードレイアウト（入力ロケール + 物理キーボード種別）を
交換し、不一致を UI に警告表示する。既存のキー転送経路は変更しない（一致環境ではそのまま動作）。

### 実装

| 箇所 | 変更 |
| --- | --- |
| `crates/tailkvm-net/src/protocol.rs` | `WireMessage::KeyboardLayout { language_id, keyboard_type, is_jis_keyboard, is_japanese_locale, label }` を追加。 |
| `crates/tailkvm-win32/src/keyboard_layout.rs` | `KeyboardLayoutInfo::mismatch_with(peer_language_id, peer_keyboard_type) -> Option<String>` を追加。入力ロケール（language_id）か物理キーボード種別（keyboard_type）が異なれば差分を含む警告文を返す。両方一致なら `None`。 |
| `apps/tailkvm-ui/src-tauri/src/lib.rs` | `TcpSessionSnapshot` に `local_keyboard_layout` / `peer_keyboard_layout` / `keyboard_layout_warning`（いずれも `Option<String>`）を追加。`send_local_keyboard_layout`（自分のレイアウトを送信）と `apply_peer_keyboard_layout`（受信レイアウトと自分を比較し snapshot 更新）を追加。 |
| 同 receiver フロー | `Hello` 受信 → `HelloAck` 送信の直後に `send_local_keyboard_layout` を送出。受信ループに `KeyboardLayout` arm を追加し `apply_peer_keyboard_layout` を呼ぶ。 |
| 同 controller フロー | `Hello` 送信直後に `send_local_keyboard_layout` を送出。受信 select ループに `KeyboardLayout` arm を追加し `apply_peer_keyboard_layout` を呼ぶ。 |
| `apps/tailkvm-ui/src/main.ts` | `TcpSessionSnapshot` 型に 3 フィールド追加。TCP state カードに不一致時の警告バナー（`error-box`）と Local/Peer layout 行を表示。 |

### 設計上のポイント

- 双方向交換: controller→receiver と receiver→controller の両方でレイアウトを送るため、
  どちら側の UI でも相手レイアウトと自分を比較した警告が出る。
- レイアウト送信失敗は **非致命的**（last_event に記録して継続）。接続自体は維持する。
- 比較は「入力ロケール (HKL low word)」と「物理キーボード種別 (GetKeyboardType)」の 2 軸。
  IME 変換モードは対象外（設計メモどおりフェーズ3 で扱う）。
- 既存のキー転送（`KeyboardKey`/`KeyboardText`）の挙動は不変。本フェーズは「警告のみ」。
- レイアウト取得タイミングは Hello 交換時で、foreground window 依存（接続時は TailKVM が
  foreground のことが多い）。keyboard_type はマシン全体なので確実。language_id の精度は
  実機実測で確認（残課題）。

### 静的検証結果

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `cargo clippy -p tailkvm-win32 -p tailkvm-net` | ✅ warning ゼロ |
| `npm run build` | ✅ exit 0 |

### 実機検証手順（Bob-note 実機が必要 — 未実施）

1. 同一レイアウト同士（例: 両方 JIS/日本語）で接続。
   - 期待: 警告バナー無し。Local/Peer layout 行に両者の label が表示され、
     `last_event` が `Keyboard layout match. ...`。
2. 異なるレイアウト（例: US controller ↔ JIS receiver）で接続。
   - 期待: 両端の UI に `⚠ Keyboard layout mismatch: input locale (...); physical keyboard type (...)`
     の警告バナーが表示される。
3. controller / receiver いずれの役割でも警告が出ること（双方向交換の確認）。
4. レイアウト送信に失敗しても接続・マウス/キー転送が継続すること。

### 結論

- レイアウト情報交換と不一致警告（設計メモ フェーズ1）を実装。静的検証はすべて成功。
- 既存のキー転送は不変。残課題: 実機での双方向警告表示と language_id 実測、フェーズ2
  （`ToUnicodeEx` ベースの文字解決）への接続。

---

## Cycle 1 / Task T1: tailkvm-net protocol serialization round-trip tests

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8) — subagent-based PDCA セッション
- 種別: Test / 自動テスト追加（プロダクションコード変更なし）

### 目的

controller↔receiver の wire contract（serde tag `type` の snake_case、全フィールド、
UTF-16 サロゲートペアテキスト、改行フレーミング）を固定し、将来の serde / rename 変更で
互換性が黙って壊れるのを防ぐ。最小リスク・最も独立した対象でテストハーネスを確立する。

### 実装内容

`crates/tailkvm-net/src/protocol.rs` に `#[cfg(test)] mod tests` を追加（本体は無変更）。

- `assert_roundtrip(message)`: encode→改行フレーミング検証→改行除去→decode→
  `serde_json::to_value` の正準 JSON 比較で全フィールド一致を確認。
  （`WireMessage` は `PartialEq` 非導出のため直接比較せず JSON 比較。プロダクション変更を回避。）
- `roundtrip_all_variants`: 全 13 variant（Hello/HelloAck/Heartbeat/HeartbeatAck/
  MouseSetPosition/MousePosition/MouseMove/MouseButton/MouseWheel/KeyboardText/
  KeyboardKey/KeyboardLayout/Disconnect）を round-trip。KeyboardText に日本語 + 絵文字を含む。
- `mouse_move_uses_snake_case_tag_and_fields` / `keyboard_key_uses_snake_case_tag_and_fields`:
  `"type":"mouse_move"` / `"keyboard_key"` と各フィールド名を固定（rename 回帰検出）。
- `keyboard_text_preserves_surrogate_pairs`: astral-plane 文字（🚀）の保存を明示検証。
- `decode_line_rejects_invalid_json` / `decode_line_rejects_unknown_message_type`:
  不正 JSON・未知 type が黙って既知 variant に化けないことを確認。

### 変更ファイル

- `crates/tailkvm-net/src/protocol.rs`（test module 追加のみ）
- `TASK_LOG.md`（Current Code Analysis + 本エントリ）

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo test -p tailkvm-net` | ✅ **6 passed; 0 failed** |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0、6 modules |

### commit / push

- commit hash: `acf18d9`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目

- なし（純粋ロジック・自動テストのみ。実機不要）。

### 受け入れ条件の達成

- ✅ 全 variant round-trip + tag 形式 + Unicode + decode エラーをカバー
- ✅ 全テスト pass、check / build グリーン維持
- ✅ failsafe / hook / firewall コード無変更

### 新たに見つかった課題（分析中に発見）

- (med) receiver 側に stuck key/button の独立解放セーフティネットが無い（TCP 切断時のキー押しっぱなし）。
- (low) `start_keyboard_hook_forwarding` 引数 9 個（`too_many_arguments`）。

### 次の推奨タスク

- Cycle 2: `lib.rs` の edge/position マッピング純関数のユニットテスト
  （`is_cursor_at_edge` / `remote_entry_position` / `local_return_position` /
  `is_remote_return_edge` / `normalize_edge` / `key_to_test_key`）。実機不要、回帰検出価値が高い。

---

## Cycle 2 / Task T2: edge / remote-entry / return-edge マッピングのユニットテスト

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8) — subagent-based PDCA セッション
- 種別: Test / 自動テスト追加（プロダクションコード変更なし）

### 目的

remote mode 切替を駆動する純粋ジオメトリを固定する: エッジ検出、リモート進入点マッピング
（解像度/アスペクト比のブリッジ）、ローカル復帰点、復帰エッジ検出、エッジ正規化、テストキー写像。
これらは「上下左右配置 / 解像度差吸収 / 仮想スクリーン座標」要件を体現し、回帰すると
カーソルが誤った位置へ飛ぶ・切替/復帰に失敗する。

### 実装内容

`apps/tailkvm-ui/src-tauri/src/lib.rs` 末尾に `#[cfg(test)] mod tests` を追加（本体無変更）。
private 関数・型へ同一クレート内からアクセス。`RectI32` は `new` が private のため
pub フィールドを直接構築するヘルパ `rect()` を用意。`CursorPosition` は `PartialEq` 非導出のため
x/y を個別 assert。

| テスト | 検証内容 |
| --- | --- |
| `normalize_edge_keeps_valid_and_defaults_to_right` | 有効値保持・trim/大小無視・未知→"right" |
| `is_cursor_at_edge_respects_margin_on_each_side` | 4 辺それぞれ margin 境界の on/off |
| `is_cursor_at_edge_handles_negative_origin_virtual_screen` | 原点非 (0,0) のマルチモニタ仮想スクリーン |
| `remote_entry_position_enters_opposite_edge_with_aspect_mapping` | switch 辺の反対辺から進入 + 比率マッピング |
| `remote_entry_position_clamps_ratio_within_bounds` | 範囲外カーソルでも [0, remote-1] にクランプ |
| `local_return_position_uses_safe_margin_floor_of_8` | safe_margin = max(margin, 8) を 4 辺で確認 |
| `is_remote_return_edge_mirrors_switch_edge` | 復帰辺が進入辺を mirror（4 方向、margin 床 8） |
| `key_to_test_key_maps_known_keys_and_extended_flags` | 既知キーの vk/extended・未知→None |

### 変更ファイル

- `apps/tailkvm-ui/src-tauri/src/lib.rs`（test module 追加のみ）
- `TASK_LOG.md`（T1 commit hash 追記 + 本エントリ）

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo test -p tailkvm-ui` | ✅ **8 passed; 0 failed** |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0、6 modules |

### commit / push

- commit hash: `5974af8`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目

- なし（純粋ロジック・自動テストのみ。実機不要）。

### 受け入れ条件の達成

- ✅ 6 関数すべてをカバー（負原点仮想スクリーン・クランプ含む）
- ✅ 全テスト pass、check / build グリーン維持
- ✅ hook / failsafe / firewall コード無変更

### 次の推奨タスク

- Cycle 3: `keyboard_layout.rs` の `mismatch_with` ユニットテスト
  （locale 一致/不一致、keyboard_type 一致/不一致、両不一致、両一致→None）。実機不要。

---

## Cycle 3 / Task T3: keyboard_layout::mismatch_with ユニットテスト

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8) — subagent-based PDCA セッション
- 種別: Test / 自動テスト追加（プロダクションコード変更なし）

### 目的

JIS/US × 入力ロケールの不一致検出（Task 9D フェーズ1 ロジック、UI 警告バナーを駆動）を固定する。
回帰すると誤警告を量産するか、記号キーが壊れる実不一致を黙って隠す恐れがある。

### 実装内容

`crates/tailkvm-win32/src/keyboard_layout.rs` 末尾に `#[cfg(test)] mod tests` を追加（本体無変更）。
`KeyboardLayoutInfo` は全フィールド pub のためヘルパ `layout(language_id, keyboard_type)` で直接構築。

| テスト | 検証内容 |
| --- | --- |
| `no_warning_when_both_axes_match` | JIS↔JIS / US↔US で `None` |
| `warns_on_input_locale_difference_only` | locale のみ差: "input locale" を含み keyboard type は含まない、両端 0x0409/0x0411 を表示 |
| `warns_on_keyboard_type_difference_only` | keyboard_type のみ差: "physical keyboard type" を含み locale は含まない |
| `warns_on_both_axes_and_lists_both` | 両差: 両方の文言 + "Keyboard text" フォールバック案内 |

### 変更ファイル

- `crates/tailkvm-win32/src/keyboard_layout.rs`（test module 追加のみ）
- `TASK_LOG.md`（T2 commit hash 追記 + 本エントリ）

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo test -p tailkvm-win32` | ✅ **4 passed; 0 failed** |
| `cargo test --workspace` | ✅ **18 passed; 0 failed**（net 6 / ui 8 / win32 4 + core 既存） |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: `03a90ee`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目

- なし（純粋ロジック・自動テストのみ。実機不要）。

### 受け入れ条件の達成

- ✅ match→None / locale 単独 / keyboard_type 単独 / 両差 をカバー
- ✅ 全テスト pass、workspace 全体 18 passed、check / build グリーン
- ✅ FFI / hook / failsafe / firewall コード無変更

### 次の推奨タスク

- Cycle 4: receiver 側 stuck key/button セーフティネット（分析で発見した med 課題）。
  ただし receiver injection 経路に触れるため、まず設計を docs に書いてから小さく実装する
  （`handle_receiver_stream` の接続終了時に押下中キー/ボタンを解放）。実装は次セッションでも可。
  代替の純テスト枠が尽きた場合は、stuck key/button 解放ヘルパを純関数として切り出し→テスト追加。

---

## Cycle 4 / Task T4: stuck-key/button トラッキングヘルパの抽出 + テスト

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8) — subagent-based PDCA セッション
- 種別: Refactor + Test（挙動保存リファクタ。安全関連経路のテスト容易化）

### 目的

「キャプチャ停止時に押下中のキー/ボタンをちょうど 1 回だけ解放する」安全性を担保する
押下トラッキングは、これまで spawn 内 async closure にインラインで書かれ単体テスト不能だった。
挙動を保ったまま純関数 `track_button_press` / `track_key_press` に抽出し、
dedup・複数解放・未押下解放（no-op）を単体テストする（ユーザ要望「no stuck button/key helper tests」）。

### 実装内容

`apps/tailkvm-ui/src-tauri/src/lib.rs`:

- `track_button_press(&mut Vec<String>, button: &str, down: bool)` を追加。
  down 時に未登録なら push（重複 down は無視）、up 時に retain で除去。
- `track_key_press(&mut Vec<(u16,u16,bool)>, vk, scan_code, extended, down)` を追加。
  `(vk, scan_code, extended)` をキーに同様の dedup。
- mouse hook 転送 closure のインライン押下トラッキング（10 行）を `track_button_press` 呼び出しに置換。
- keyboard hook 転送 closure のインライン押下トラッキング（11 行）を `track_key_press` 呼び出しに置換。
- **挙動は完全保存**（down=未登録時のみ push、up=該当除去、解放経路の drain は不変）。
- test module に `track_button_press_dedups_and_releases` /
  `track_key_press_dedups_by_vk_scan_extended` を追加。

### 安全性

- failsafe / ローカル抑制（`return 1`）/ firewall には一切触れていない。
- 解放経路（ループ終了時の `pressed_*.drain(..)` → KeyUp/ButtonUp 送出）は無変更。
- リファクタはトラッキング集合の構築ロジックのみで、送信・抑制・停止条件は不変。

### 変更ファイル

- `apps/tailkvm-ui/src-tauri/src/lib.rs`（ヘルパ 2 関数 + インライン置換 2 箇所 + テスト 2 件）
- `TASK_LOG.md`（T3 commit hash 追記 + 本エントリ）

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo test -p tailkvm-ui` | ✅ **10 passed; 0 failed**（T2 の 8 + 本 2） |
| `cargo test --workspace` | ✅ **21 passed; 0 failed**（core 1 / net 6 / ui 10 / win32 4） |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: `e8b47fb`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目

- 実機での実際の stuck key/button 解放（hook 経由）は従来どおり実機 2 台が必要（手順は Task 9B-1 に記載）。
  本タスクは集合トラッキングの純ロジックを検証したもので、SendInput 注入自体は未検証のまま。

### 受け入れ条件の達成

- ✅ 挙動保存（dedup on down / remove on up / 未押下解放 no-op をテストで固定）
- ✅ 新テスト pass、workspace 全体 21 passed、check / build グリーン
- ✅ failsafe / 抑制ロジック無変更

### 新たに見つかった課題（再掲・未対応）

- (med) receiver 側 stuck key/button セーフティネット（TCP 切断時）。controller 側トラッキングが
  テスト可能になったので、次は receiver 側に「接続終了時に全押下解放」を設計→実装するのが自然。

### 次の推奨タスク

- Cycle 5（次セッション推奨）: receiver 側 disconnect 安全解放の **設計メモ**作成
  （`docs/receiver-stuck-input-safety.md`）。実装前に設計を固定し、small PoC に限定する。
  → 本セッションの Cycle 5 で **小さく実装**した（下記）。

---

## Cycle 5 / Task T5: receiver 側 stuck-input セーフティネット（切断時解放）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8) — subagent-based PDCA セッション
- 種別: Do / 安全性実装（分析で発見した med 課題の修正）

### 目的

controller がキー/ボタン押下中に死んだ場合、receiver（Bob-note）側でそのキー/ボタンが
押しっぱなしになる課題を修正。受信メッセージから押下状態を追跡し、受信ループ終了時
（Disconnect / EOF / read error のいずれの経路でも）に残っている押下を全解放する。

### なぜ安全か（lockout リスク評価）

- **receiver は hook も入力抑止も持たない**（注入専用）。よって余分な KeyUp/ButtonUp を
  合成しても receiver を締め出すことは原理的に不可能。
- 既に離されているキーへの KeyUp は Windows 上で無害（冪等）。
- failsafe / firewall / ローカル抑止（controller 側の `return 1`）には一切触れていない。

### 実装

`apps/tailkvm-ui/src-tauri/src/lib.rs` の `handle_receiver_stream` のみ変更:

- 受信ループ前に `held_keys: Vec<(u16,u16,bool)>` / `held_buttons: Vec<String>` を宣言。
- `KeyboardKey` 受信時に `track_key_press(&mut held_keys, vk, scan_code, extended, down)`、
  `MouseButton` 受信時に `track_button_press(&mut held_buttons, &button, down)` を呼ぶ
  （**T4 で抽出・テスト済みの純ヘルパを再利用**）。
- ループ終了後（全 break の合流点）に held_keys/held_buttons を drain し、
  `send_key_event(.., false, ..)` / `send_mouse_button(.., false)` で解放。解放件数を last_event に表示。
- `KeyboardText` は down+up を即時完結注入するため追跡不要。MouseMove/Wheel/SetPosition も状態を持たない。

### 変更ファイル

- `apps/tailkvm-ui/src-tauri/src/lib.rs`（`handle_receiver_stream` のみ）
- `TASK_LOG.md`（T4 commit hash 追記 + 本エントリ）

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `cargo test --workspace` | ✅ **21 passed; 0 failed**（追跡ヘルパは T4 で検証済み） |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: `a3d2fbd`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目（Manual Verification Required — 実機 2 台）

1. **正常切断時の解放**: controller でキー押下中（例: Shift 長押し）に controller アプリを終了
   → receiver 側で Shift が押しっぱなしにならず、`last_event` に
   `Released N stuck key(s)...` が表示されること。
2. **ネットワーク断**: Tailscale を切断 → receiver の read error 経路でも解放されること。
3. **マウスボタン**: 左ボタン down 中に切断 → receiver でドラッグ状態が残らないこと。
4. 解放が冪等であること（既に離されたキーへの KeyUp で誤動作しない）。

> 純ロジック（追跡 dedup）は T4 で自動検証済み。SendInput 注入自体は実機が必要なため上記は未検証。

### 受け入れ条件の達成

- ✅ T4 のテスト済みヘルパを再利用した追跡
- ✅ 全 exit 経路（Disconnect/EOF/error）の合流点で解放
- ✅ compile / 21 tests / build グリーン
- ✅ failsafe / firewall / 抑止ロジック無変更、lockout リスクなし

### 次の推奨タスク

- Task 11（clipboard sharing foundation）の設計 + テキスト最小実装、または
- Task 10（Raw Input mouse capture 調査）の `docs/raw-input-mouse-design.md` 作成。
- いずれも実機注入は手動検証、純ロジック（無限同期ループ防止のシーケンス番号判定など）は
  ユニットテスト可能。

---

## Overnight Summary（2026-06-02 subagent-based PDCA セッション）

### セッションの方針メモ

- ユーザ指定のサブエージェント（codebase-analyst / safety-reviewer / input-hook-specialist 等）は
  **本環境の巨大な base context（数百の MCP ツール/skill 注入）により "Prompt is too long" で起動不可**だった。
  そのため main Claude が各レンズを直接適用して分析・統合判断を行った
  （「最終統合判断は main のみ」「Rust backend は main が編集」の方針には合致）。
- 全サイクルで **テスト基盤確立 → 安全関連経路の強化** を最小リスク順に実施。
  main へ push せず / force push せず / 破壊的操作なし / failsafe 不変 を厳守。

### 実施タスク一覧（全 5 サイクル）

| Cycle | Task | 種別 | 内容 | commit |
| --- | --- | --- | --- | --- |
| 0 | 健全性確認 | Check | fmt / check / build 全グリーン確認、Current Code Analysis 作成 | （T1 に同梱） |
| 1 | T1 | Test | tailkvm-net protocol serialization round-trip tests（6 件） | `acf18d9` |
| 2 | T2 | Test | edge/remote-entry/return-edge マッピング純関数テスト（8 件） | `5974af8` |
| 3 | T3 | Test | keyboard_layout::mismatch_with テスト（4 件） | `03a90ee` |
| 4 | T4 | Refactor+Test | stuck-key/button トラッキングヘルパ抽出 + テスト（2 件） | `e8b47fb` |
| 5 | T5 | Feat（安全） | receiver 切断時の stuck-input 解放 | `a3d2fbd` |

### commit 一覧（このセッション）

```
a3d2fbd feat: release stuck keys/buttons on receiver disconnect (Task T5)
e8b47fb refactor: extract and test stuck-key/button tracking helpers (Task T4)
03a90ee test: add keyboard layout mismatch_with unit tests (Task T3)
5974af8 test: add edge/position mapping unit tests (Task T2)
acf18d9 test: add protocol serialization round-trip tests (Task T1)
```

### push 結果

- すべて `claude/pdca-tailkvm-software-kvm` へ push 完了。**main への push / force push は一切なし**。

### ビルド結果（最終状態）

| チェック | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ |
| `cargo test --workspace` | ✅ **21 passed; 0 failed**（開始時は実質 0 → +20 件） |
| `npm run build` | ✅ exit 0 |
| `npm run tauri build` | ⏸ 未実行（今回はテスト基盤と安全性に集中。インストーラ生成は次回） |

### テストカバレッジの変化

- 開始時: `tailkvm-core::add` プレースホルダのみ（実質 0）。
- 終了時: **20 件の意味あるテスト**を追加（protocol 6 / edge-mapping 8 / layout 4 / stuck-tracking 2）。
  wire contract・remote 切替ジオメトリ・レイアウト不一致・stuck 解放の回帰を自動検出できるようになった。

### 発見した重大課題

1. **(med・本セッションで修正)** receiver 側に stuck key/button の解放セーフティネットが無かった
   → T5 で実装。controller 切断時に Bob-note でキー/ボタンが押しっぱなしになる問題を解消。
2. **(low・未対応)** `start_keyboard_hook_forwarding` の引数 9 個（`too_many_arguments`）。
   フック関連フィールドを構造体にまとめると解消可能（機能影響なし）。

### 次にユーザーが手動確認すべきこと（実機 2 台: 操作元 + Bob-note）

- **最優先**: T5 の動作確認 — controller でキー/マウスボタン押下中に
  (a) controller アプリ終了、(b) Tailscale 切断 のそれぞれで、Bob-note 側でキー/ボタンが
  押しっぱなしにならず `last_event` に `Released N stuck key(s)...` が出ること。
- 既存タスク（9B-1/9B-2/9C/9D phase1）の実機手順は各エントリに記載済み（未検証のまま）。
- 自動テストは CI 等で `cargo test --workspace` を回せば回帰検出可能（実機不要）。

### 次の推奨タスク（優先順）

1. **Task 11 clipboard sharing foundation** — テキストクリップボード送受信の最小実装。
   無限同期ループ防止（送信元ハッシュ/シーケンス判定）は純ロジックとしてユニットテスト可能。
2. **Task 10 Raw Input mouse 調査** — `docs/raw-input-mouse-design.md` を先に作成（設計のみ）。
3. **low 課題の解消** — `start_keyboard_hook_forwarding` の引数を構造体化（warning 予防・可読性）。
4. **`npm run tauri build`** で Bob-note 検証用インストーラ生成（GitHub Release は明示承認まで行わない）。

---

# Session 2（継続）— GitHub Release 承認後

ユーザが GitHub Release 作成を明示承認。推奨タスク（Task 11 → Task 10 → low 課題 → tauri build →
インストーラ → Release）を進め、最後に「この端末 1 台での動作テスト方法論の確立 + 環境構築 + 実行」を行う。

## Cycle 6 / Task 11: クリップボード共有の基盤（テキスト）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Do / 実装（テキストクリップボード送受信 + echo ループ防止の純ロジック基盤）

### 目的

テキストクリップボードを peer へ送れるようにする最小実装。無限同期ループ（echo）防止の
テスト済み純ロジック基盤を先に用意する。画像/ファイルは設計のみ（スコープ外）。

### 実装

| 箇所 | 変更 |
| --- | --- |
| `crates/tailkvm-win32/Cargo.toml` | windows-sys に `Win32_System_DataExchange` / `Win32_System_Memory` features 追加。 |
| `crates/tailkvm-win32/src/clipboard.rs`（新規） | `get_clipboard_text()` / `set_clipboard_text()`（CF_UNICODETEXT、`ClipboardSession` RAII で必ず CloseClipboard）。`ClipboardLoopGuard`（content hash で自分の echo を抑止する純ロジック）+ `content_hash()`。 |
| `crates/tailkvm-win32/src/lib.rs` | `pub mod clipboard;` 追加。 |
| `crates/tailkvm-net/src/protocol.rs` | `WireMessage::ClipboardText { text }` 追加 + roundtrip テストに 1 ケース追加。 |
| `apps/tailkvm-ui/src-tauri/src/lib.rs` | `AppState.clipboard_guard` 追加。`send_clipboard_text` コマンド（ローカルクリップボード読取→echo guard 判定→`ClipboardText` 送出、10 万文字上限）。receiver に `ClipboardText` arm（`set_clipboard_text`）。invoke_handler 登録。 |
| `apps/tailkvm-ui/src/main.ts` | "Send clipboard to peer" ボタン + 配線。 |

### 設計上のポイント / 安全性

- **echo ループ防止**: `ClipboardLoopGuard` が「自分が送信/適用した内容のハッシュ」を保持し、
  同一内容の再送を抑止。今回は手動 push（controller→receiver 一方向）なので原理的にループしないが、
  将来のクリップボード監視（自動同期）に向けた基盤を**テスト可能な純ロジック**として先置き。
- 受信側 guard 配線と自動監視は将来タスク（auto-sync）として明記。
- failsafe / firewall / 入力抑止ロジックには一切触れていない。
- `SetClipboardData` 成功時はシステムが hglobal を所有（free しない）点をコメント明記。

### 変更ファイル

- 上記 6 ファイル + `TASK_LOG.md`

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0、warning ゼロ（clipboard FFI コンパイル確認） |
| `cargo test --workspace` | ✅ **24 passed; 0 failed**（core 1 / net 6 / win32 7 / ui 10） |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: `7b833ca`
- push: claude/pdca-tailkvm-software-kvm へ push 完了（main へは push せず）

### 未検証項目（Manual Verification Required — この端末で可、後述の Cycle 9 で実施）

1. controller で何かテキストをコピー → "Send clipboard to peer" → receiver 側でクリップボードに反映
   （メモ帳に Ctrl+V で確認）。
2. 空クリップボード / 非テキスト時にエラー文言が出ること。
3. 同一内容を連続送信 → 2 回目が "Clipboard unchanged..." でスキップされること（echo guard）。

### 受け入れ条件の達成

- ✅ テキストクリップボード送受信を配線
- ✅ `ClipboardLoopGuard` の echo 抑止を 3 ユニットテストで検証
- ✅ 画像/ファイルはスコープ外として設計コメント明記
- ✅ check/test/build グリーン、failsafe/firewall 不変

### 次の推奨タスク

- Cycle 7: Task 10 Raw Input mouse capture 調査メモ（`docs/raw-input-mouse-design.md`）。

## Cycle 7 / Task 10: Raw Input マウスキャプチャ調査・設計メモ

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Plan / 設計ドキュメント（コード変更なし）

### 成果物

`docs/raw-input-mouse-design.md`（新規）。

### 内容要約

- 現行 `GetCursorPos`+`SetCursorPos` warp 方式の問題（ちらつき・warp 誤検出・量子化・ポインタ加速二重適用）を整理。
- Raw Input（`WM_INPUT`/`RAWMOUSE.lLastX/Y`）で HID 生の相対量を取得する利点と API（`RegisterRawInputDevices`,
  `RIDEV_INPUTSINK`/`RIDEV_NOLEGACY`/`RIDEV_REMOVE`）。
- 役割分担表（SendInput=注入 / low-level hook=捕捉+抑止 / Raw Input=高精度相対量）。
- 段階導入: **フェーズ A**（lock 維持 + 相対量だけ Raw Input 置換、低リスク・opt-in 推奨）、
  **フェーズ B**（`RIDEV_NOLEGACY` で warp 廃止、failsafe 停止保証を固めてから）。
- failsafe（Ctrl+Alt+Pause）維持の必須要件、PoC スコープ・受け入れ条件、未解決事項を明記。

### 実行コマンドと結果（回帰確認のみ）

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` / `cargo check --workspace` | ✅ exit 0 |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: `bf13bb0`
- push: claude/pdca-tailkvm-software-kvm 完了（main へは push せず）

### 次の推奨タスク

- Cycle 8: low 課題 — `start_keyboard_hook_forwarding` の引数（9 個）を構造体化する挙動保存リファクタ。

## Cycle 8 / low 課題: too_many_arguments の解消

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Refactor（挙動保存）+ lint 解消

### 目的

clippy `too_many_arguments` を解消する。session 1 で指摘した `start_keyboard_hook_forwarding`（9 引数）を
構造体化し、もう 1 件の 9 引数関数 `start_mouse_capture`（Tauri command）を境界として明示 allow する。

### 実装

- `KeyboardForwardingContext` 構造体を追加（AppState 由来の 7 つの共有ハンドルを束ねる）+
  `AppState::keyboard_forwarding_context()` ヘルパ。
- `start_keyboard_hook_forwarding` の引数を `(ctx, sender, label)` の 3 個に削減。
  **関数本体は不変**（先頭で `ctx` から同名ローカルへ clone 再束縛し、以降は byte-for-byte 同一）。
- 呼び出し 2 箇所を更新（command 側は `&state.keyboard_forwarding_context()`、
  capture ループ側は in-scope の clone から `KeyboardForwardingContext` を構築）。
- `start_mouse_capture` は Tauri IPC の引数契約（フロントが named args で invoke）であり構造体化すると
  invoke 署名が壊れるため、コメント付き `#[allow(clippy::too_many_arguments)]` を付与。

### 挙動保存・安全性

- `start_keyboard_hook_forwarding` は本体不変・ローカル名不変のため挙動完全保存。
- failsafe / firewall / 入力抑止には触れていない。

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` | ✅ exit 0 |
| `cargo check --workspace` | ✅ exit 0 |
| `cargo clippy -p tailkvm-ui` | ✅ `too_many_arguments` 解消（6→5 warnings、残りは session 1 既知の style lint） |
| `cargo test --workspace` | ✅ 24 passed; 0 failed |
| `npm run build` | ✅ exit 0 |

### commit / push

- commit hash: （本コミットで記録）
- push: claude/pdca-tailkvm-software-kvm（main へは push しない）

### 残存（out of scope・session 1 既知）

- `tailkvm-ui` の style lint 5 件（`manual_is_multiple_of` ×3、`match` 単一パターン、needless ref）。
  機能影響なし。必要なら別タスクで `clippy --fix`。

### 次の推奨タスク

- Cycle 9: 単体マシン動作テスト方法論の確立 + 環境構築 + 実行（loopback 統合テスト + 手動 GUI 手順）。
- Cycle 10: `npm run tauri build` → インストーラ生成 → GitHub Release（承認済み）。

## Cycle 9 / 単体マシン動作テスト方法論の確立 + 環境構築 + 実行

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Test（自動統合テスト追加 + 実機実行）+ Docs

### 目的

「この端末 1 台で動作テストする」方法論を 3 層（L1 トランスポート / L2 注入 FFI / L3 GUI スモーク）で確立し、
自動化可能な L1・L2 を実際に構築・実行する。

### 実装・成果物

1. **L1 ループバック統合テスト**（`crates/tailkvm-net/tests/loopback.rs` 新規、tokio を dev-dependency 追加）:
   `127.0.0.1` 上で writer↔reader を繋ぎ、全 `WireMessage` を `encode_line`→TCP→`BufReader::lines()`+
   `decode_line` で往復し正準 JSON 一致を検証。個別送信版 + 1 回 write 連結版（coalescing 下のフレーミング）。
2. **L2 クリップボード実 FFI テスト**（`crates/tailkvm-win32/tests/clipboard_roundtrip.rs` 新規、`#[ignore]`）:
   実 Windows クリップボードで set→get の Unicode/絵文字往復を検証。
3. **方法論ドキュメント**（`docs/single-machine-testing.md` 新規）: 3 層の説明、localhost ガードの仕様
   （`start_mouse_capture` は 127.* 拒否だが個別送信・receiver 注入は localhost で動く）、L3 手動手順、
   1 台では検証不可な項目（移動キャプチャ/画面端/抑止/Tailscale/T5）を明記。

### 実行結果（この端末で実行）

| コマンド | 結果 |
| --- | --- |
| `cargo test -p tailkvm-net --test loopback` | ✅ **2 passed**（実 TCP ループバック動作確認） |
| `cargo test -p tailkvm-win32 --test clipboard_roundtrip -- --ignored` | ✅ **1 passed**（実クリップボード FFI が Unicode/絵文字を完全往復） |
| `cargo test --workspace`（既定） | ✅ **26 passed; 0 failed; 1 ignored**（core1/net 6+loopback2/ui10/win32 7） |
| `cargo fmt --all` / `cargo check --workspace` / `npm run build` | ✅ 全 exit 0 |

### この端末で「実証済み」になったこと

- ✅ wire トランスポート（TCP + 改行フレーミング）の controller↔receiver 往復。
- ✅ クリップボード Win32 FFI（Task 11）の実機動作（set/get Unicode）。
- L3（キー/マウス/クリップボードのアプリ経由注入）は手動手順を `docs/single-machine-testing.md` に明記。

### commit / push

- commit hash: （本コミットで記録）
- push: claude/pdca-tailkvm-software-kvm（main へは push しない）

### 次の推奨タスク

- Cycle 10: `npm run tauri build` → インストーラ生成確認 → GitHub Release（ユーザ承認済み）。

## Cycle 10 / インストーラ生成 + GitHub Release（承認済み）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Deploy（インストーラ生成 + リリース準備）

### 実施

- `npm run tauri build` を実行（release プロファイル、35.3s でビルド完了）。
- **2 種のインストーラを生成**:
  - MSI: `target\release\bundle\msi\TailKVM_0.1.0_x64_en-US.msi`（3.31 MB）
  - NSIS: `target\release\bundle\nsis\TailKVM_0.1.0_x64-setup.exe`（2.12 MB）
- リリースノート `docs/release-notes-v0.1.0-bobnote.md` を作成（バージョン管理下）。

### GitHub Release の状態 — ✅ 作成完了（2026-06-02、デバイスフロー認証後）

- ユーザ依頼で `gh auth login`（デバイスフロー）を実行。ワンタイムコードをユーザに伝達 → ブラウザ承認で
  `Panda17TK` として認証完了。
- `gh release create` で **プレリリースを公開**:
  - URL: https://github.com/Panda17TK/TailKVM/releases/tag/v0.1.0-bobnote-1
  - tag: `v0.1.0-bobnote-1`、prerelease: true、target: `claude/pdca-tailkvm-software-kvm`（**main 不使用**）
  - 添付資産（uploaded 確認済み）: `TailKVM_0.1.0_x64_en-US.msi` / `TailKVM_0.1.0_x64-setup.exe`
  - notes: `docs/release-notes-v0.1.0-bobnote.md`

### 手動で Release を作成するコマンド（認証後）

```powershell
cd V:\src\tailkvm
gh auth login            # 一度だけ。ブラウザ/トークンで認証
gh release create v0.1.0-bobnote-1 `
  "target\release\bundle\msi\TailKVM_0.1.0_x64_en-US.msi" `
  "target\release\bundle\nsis\TailKVM_0.1.0_x64-setup.exe" `
  --target claude/pdca-tailkvm-software-kvm `
  --title "TailKVM v0.1.0 (Bob-note verification build)" `
  --notes-file docs/release-notes-v0.1.0-bobnote.md `
  --prerelease
```

> `--target` に作業ブランチを指定（main へは触れない）。`--prerelease` で未検証プレリリースを明示。
> タグ `v0.1.0-bobnote-1` はコマンドが自動作成（既存タグ・main への push なし）。

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `npm run tauri build` | ✅ MSI + NSIS 生成（合計 2 bundles） |
| `gh --version` / `gh auth status` | gh 2.93.0、**未認証**（Release は認証後に手動完了） |

### commit / push

- commit hash: （本コミットで記録、リリースノート + 本ログ）
- push: claude/pdca-tailkvm-software-kvm（main へは push しない）。インストーラ本体は target/ 配下で gitignore（コミットしない）。

### 次にユーザーがすべきこと

1. `gh auth login` 後、上記コマンドで Release 作成（または GitHub Web UI で 2 ファイルを添付）。
2. Bob-note へインストーラ配布 → `docs/single-machine-testing.md` / 各タスクの 2 台手順で実機検証。

---

## Session 2 Summary（2026-06-02・GitHub Release 承認後）

### 実施タスク一覧（Cycle 6–10）

| Cycle | Task | 種別 | 内容 | commit |
| --- | --- | --- | --- | --- |
| 6 | Task 11 | Feat | クリップボードテキスト共有 + echo ループ防止基盤（テスト済） | `7b833ca` |
| 7 | Task 10 | Plan | Raw Input マウス調査・設計メモ | `bf13bb0` |
| 8 | low 課題 | Refactor | `too_many_arguments` 解消（context 構造体 + command allow） | `938fad0` |
| 9 | テスト方法論 | Test+Docs | 単体マシン loopback 統合テスト + 実クリップボード FFI テスト + 方法論 doc | `d3cc215` |
| 10 | Task 10(配布) | Deploy | `npm run tauri build` で MSI/NSIS 生成、Release 準備 | `6b22973` |

### commit 一覧（Session 2）

```
6b22973 docs: add v0.1.0 release notes and record installer build (Task 10/Cycle 10)
d3cc215 test: add single-machine loopback + clipboard FFI tests and methodology
938fad0 refactor: resolve clippy too_many_arguments on hook forwarding
bf13bb0 docs: add Raw Input mouse capture design memo (Task 10)
7b833ca feat: add clipboard text sharing foundation (Task 11)
```

### push 結果

- すべて `claude/pdca-tailkvm-software-kvm` へ push 完了。**main への push / force push なし**。

### ビルド / テスト結果（最終）

| 項目 | 結果 |
| --- | --- |
| `cargo fmt --all` / `cargo check --workspace` | ✅ |
| `cargo test --workspace` | ✅ 26 passed; 0 failed; 1 ignored |
| `cargo test ... clipboard_roundtrip -- --ignored` | ✅ 1 passed（実 Windows クリップボード往復） |
| `cargo clippy -p tailkvm-ui` | ✅ `too_many_arguments` 解消（残 5 件は既知 style lint） |
| `npm run build` | ✅ |
| `npm run tauri build` | ✅ MSI 3.31MB + NSIS 2.12MB 生成 |

### テスト総数の推移

- Session 1 開始: 実質 0 → Session 2 終了: **自動 26 + on-demand 1（実 FFI）= 27**。
  protocol(6) / loopback(2) / edge-mapping(8) / layout(4) / stuck-tracking(2) / clipboard guard(3) /
  clipboard 実 FFI(1, ignored) / core(1)。

### この端末で「実証済み」

- ✅ TCP トランスポート + 改行フレーミングの controller↔receiver 往復（loopback 実 TCP）。
- ✅ クリップボード Win32 FFI（set/get Unicode・絵文字）の実機動作。
- ✅ MSI / NSIS インストーラのビルド成功。

### 重大な残課題 / ブロッカー

- なし（GitHub Release はデバイスフロー認証後に公開完了:
  https://github.com/Panda17TK/TailKVM/releases/tag/v0.1.0-bobnote-1 ）。

### 次にユーザーが手動で行うこと

1. Bob-note へインストーラ配布（上記 Release から DL）→ 2 台でのみ検証可能な項目を実機確認
   （マウス移動キャプチャ/画面端切替/WH_*_LL ローカル抑止/Tailscale 越し疎通/Firewall rule/T5 stuck-key 解放）。
2. 2 台検証 OK 後、Raw Input フェーズ A PoC（`docs/raw-input-mouse-design.md`）や
   IME/半角全角/Win/Alt+Tab 実装（`docs/keyboard-layout-ime-design.md`）へ。
3. 2 台検証 OK 後、Raw Input フェーズ A PoC（`docs/raw-input-mouse-design.md`）や
   IME/半角全角/Win/Alt+Tab 実装（`docs/keyboard-layout-ime-design.md`）へ。

## Cycle 11 / 実装精査 + 品質・パフォーマンス・UX リファクタ

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Audit + Refactor（簡易実装/課題/パフォーマンス/見た目/動作の精査と打ち手）

### 目的

Session 1–2 の実装を精査し、簡易実装・課題・パフォーマンス/見た目/動作への影響を洗い出して
打ち手を設計・実施する。

### 成果物

- `docs/implementation-audit-2026-06-02.md`（全 findings + 重大度 + 対応/フォロー）。

### 精査で見つけた主な点と打ち手

| ID | 重大度 | 内容 | 対応 |
| --- | --- | --- | --- |
| A1 | H | TCP_NODELAY 未設定（Nagle で入力レイテンシ） | ✅ 両ソケットに `set_nodelay(true)` (`6df5be6`) |
| A2 | M | controller outbound が MouseMove 含む全送信で Debug format スパム（~30/s、last_event ちらつき） | ✅ MouseMove は更新スキップ (`6df5be6`) |
| D1 | L | `get_app_status` が古い `"Task 5 OK"` | ✅ crate version 表示へ (`eb826b6`) |
| D2 | M | トレイ "Pause input forwarding" が no-op | ✅ `pause_all_capture` 共通化 + tray 配線（手動キルスイッチ化）(`eb826b6`) |
| A3 | M | mouse-move GetCursorPos/SetCursorPos ポーリング | 📝 Raw Input 設計済（別タスク） |
| A4 | L | hook→async の 5ms ポーリングブリッジ | 📝 フォロー |
| B1 | M | receiver が複数接続を受理 | 📝 単一接続化フォロー |
| D3 | L | `tailkvm-core::add` 未使用 | 📝 整理フォロー |

### 安全性

- Ctrl+Alt+Pause failsafe 二重経路・firewall スコープは**無変更**。リファクタは latency / UI 更新頻度 /
  停止経路の共通化のみで、抑止・注入・stuck 解放ロジックは不変。

### 実行コマンドと結果

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` / `cargo check --workspace` | ✅ exit 0 |
| `cargo test --workspace` | ✅ 26 passed; 0 failed; 1 ignored |
| `npm run build` | ✅ exit 0 |

### commit / push

- commits: `6df5be6`（perf）, `eb826b6`（refactor）, 本コミット（audit doc + log）。
- push: claude/pdca-tailkvm-software-kvm（main へは push しない）。

### 次の推奨タスク

- B1 receiver 単一接続化 → A3 Raw Input フェーズ A PoC → A4 hook チャネル化。

## Cycle 12 / 安全バグ修正 + B1 + A3 + A4

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Fix(安全) + Feat + Refactor

### S1（最優先・精査で発見した failsafe ロックアウト）— `c0a1208`

- **バグ**: mouse/keyboard フック転送タスクは failsafe / peer 切断 / controller チャネル切断で `break`
  した際、**フックハンドルを drop していなかった**（タスクが hook の Arc を未キャプチャ）。
  フック proc は `running` フラグを見ず `EVENT_SENDER` のみ参照するため、`running=false` では抑止が止まらない。
  → 手動フックキャプチャ中に **Ctrl+Alt+Pause / 切断でローカル入力が抑止されたまま＝ロックアウト**。
- **修正**: 各タスクに hook の Arc をキャプチャさせ、ループ単一 exit 点で必ずハンドルを drop（unhook）。
  外部 stop とは mutex で直列化され冪等。failsafe が両フックを確実に解放するようになった。

### B1 receiver 単一接続化（最新優先）— `5ddc12e`

- accept ループで現行セッションの cancel チャネル（`oneshot`）を保持。新しい controller 接続時に
  旧ハンドラへ通知 → 旧ハンドラは `tokio::select!` でループを抜け、**T5 の stuck 解放を実行してから**終了。
- 「最新優先」採用理由: 単一ユーザ Tailscale 前提では現実の障害はゾンビ接続。最新優先は**再接続で自己回復**でき、
  新規拒否案の「TCP タイムアウトまでロックアウト」欠点がない（分析は本会話に記載）。

### A3 Raw Input フェーズA PoC（observe-only）— `7e03819`

- `crates/tailkvm-win32/src/raw_input_mouse.rs`: message-only window + `RegisterRawInputDevices(RIDEV_INPUTSINK)`
  で HID 相対デルタ（`RAWMOUSE.lLastX/Y`）を取得。純関数 `relative_delta()`（absolute/zero を除外）をユニットテスト。
- `start/stop_raw_mouse_diagnostic` コマンド + UI ボタン: **観測専用**（カーソル移動も注入もしない）。
  remote mode には未配線（既存挙動不変）。実機で WM_INPUT パイプラインを検証するための土台。

### A4 hook 転送を recv_timeout のブロッキングスレッド化 — `ee0f151`

- 旧: async タスク + `try_recv`+`sleep(5ms)`（最大 5ms レイテンシ、~400 wakeups/s）。
- 新: 専用ブロッキングスレッド + `recv_timeout(100ms)`。イベントで即時起床（≈0ms 追加レイテンシ）、
  timeout は stop フラグ再確認のみ。`Disconnected`→break（外部 stop の hook drop を捕捉）。
  failsafe / stuck 解放 / teardown / チャネル切断処理はすべて保存。新規依存なし。

### 検証

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` / `cargo check --workspace` | ✅ |
| `cargo test --workspace` | ✅ 0 failed（win32 lib に relative_delta 3 件追加） |
| `cargo clippy --workspace` | ✅ 新規 warning なし |
| `npm run build` | ✅ |

### commit / push

- `c0a1208`(S1) / `5ddc12e`(B1) / `7e03819`(A3) / `ee0f151`(A4)。本コミット(log)。
  すべて claude/pdca-tailkvm-software-kvm。main へ push せず。

### 実機検証が必要（未検証）

- S1: 手動キーボード/マウスフックキャプチャ中に Ctrl+Alt+Pause / peer 切断 → ローカル入力が即座に復帰すること。
- B1: 2 つ目の controller 接続で 1 つ目が「replaced」ログを出し T5 解放して終了、新接続が制御を得ること。
- A3: Raw Input diagnostic でマウス移動時に delta カウントが増えること（観測のみ）。
- A4: クリック/キー/ホイールのレイテンシ低下（体感）。

### 次の推奨タスク

- A3 のフェーズB（remote mode の移動量を raw delta へ置換）を実機検証後に opt-in 実装。
- IME/半角全角/Win/Alt+Tab 実装（`docs/keyboard-layout-ime-design.md`）。

## Cycle 13 / A3 フェーズB（raw delta 配線）+ IME/半角全角/Win/Alt+Tab（フェーズ2）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Feat（いずれも opt-in、既定挙動は不変）

### A3 フェーズB: remote mode 移動量を Raw delta へ opt-in 置換 — `c808f71`

- `start_mouse_capture` に `use_raw_input`（UI チェックボックス「Raw Input mouse (PoC)」、既定 OFF）。
- ON 時: capture ループが `GetCursorPos` 差分でなく `raw_input_mouse` の HID 相対デルタを使用。
  armed 中はバッファを flush（起動時ジャンプ防止）、active 中は tick ごとに合算、カーソルは pin。
  warp ヒューリスティック不要。raw 取得失敗時は従来の warp 方式へフォールバック。

### IME/半角全角/Win/Alt+Tab: フェーズ2 ルーティング — `2bd3f60`(分類器/解決器) + `d792174`(配線)

- `crates/tailkvm-win32/src/key_class.rs`（純・ユニットテスト 5 件）: `classify_key` が
  Physical / Character / ImeLocal を判定。`keyboard::resolve_key_text`（`ToUnicodeEx`）で
  controller レイアウト文字を解決。
- キーボード転送に **character-resolution モード**を opt-in 配線（`set_resolve_characters` コマンド +
  UI チェックボックス、転送ループが live 参照）。ON 時:
  - **Win / Alt+Tab / Ctrl 系 combo / 制御・ナビ・ファンクション** → physical（scan/vk）。
  - **印字キー** → `ToUnicodeEx` 解決 → Unicode（`KeyboardText`）で **JIS/US 記号差を吸収**。
  - **半角/全角・変換・無変換・かな・Kanji** → drop（receiver IME を反転させない）。
  - dead key は physical フォールバック（stuck 解放対象）。
- 既定 OFF では従来どおり全キー scan/vk 転送（挙動不変）。
- `docs/keyboard-layout-ime-design.md` §9 に実装状況を追記。
  **かな漢字 composition（フェーズ3）は未実装**（隠しウィンドウ IME PoC が必要、要 2 台検証）。

### 検証

| コマンド | 結果 |
| --- | --- |
| `cargo fmt --all` / `cargo check --workspace` | ✅ |
| `cargo test --workspace` | ✅ 0 failed（win32 lib に key_class 5 件追加） |
| `cargo clippy --workspace` | ✅ 新規 warning なし |
| `npm run build` | ✅ |

### commit / push

- `c808f71`(Phase B) / `2bd3f60`(分類器+解決器) / `d792174`(配線) / 本コミット(docs)。
  すべて claude/pdca-tailkvm-software-kvm。main へ push せず。

### 実機検証が必要（未検証）

- A3 フェーズB: `Raw Input mouse (PoC)` ON で remote 操作がちらつかず滑らかに追従、停止で復帰。
- character-resolution ON: US↔JIS で記号（`@ [ ] : 等`）が正しく入る、Ctrl+C/Win+X/Alt+Tab が効く、
  半角/全角 で receiver の入力が壊れない。Shift+Unicode 同時の細部。
- いずれも **2 台での実測が必要**。

### 次の推奨タスク

- フェーズ3（かな漢字 IME composition 取り込み）の隠しウィンドウ PoC。
- 実機実測に基づく classify 境界の調整（CapsLock 対応、Shift 折り込みの最適化）。

### リリース

- 新機能（Raw Input mouse / Resolve characters、いずれも opt-in）を含めて
  **v0.1.0-bobnote-3** を公開（prerelease、target=作業ブランチ、MSI+NSIS uploaded）。
  https://github.com/Panda17TK/TailKVM/releases/tag/v0.1.0-bobnote-3 （`gh` 認証済み環境で作成）。

## Cycle 14 / Synergy 相当ロードマップ実装（1〜5）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Feat（大半 opt-in・既定挙動不変、純ロジックは全てユニットテスト）

Synergy 相当の「シームレス切替 + 低負荷」へ向けたロードマップ全 5 項目を、検証済みの小コミットで実装。

| 項目 | 内容 | commit | 状態 |
| --- | --- | --- | --- |
| **1 (A1/E1)** | 結合座標空間 `screen_space`（純・テスト）+ 絶対カーソル seamless 捕捉エンジン（opt-in、Raw Input 駆動、warp/相対廃止・ドリフトなし） | `863db42` / `4e3dd58` | 実装（opt-in） |
| **2 (C1/C3)** | `SwitchGuard`（dwell + dead-corner、純・テスト）+ ClipCursor によるカーソル confine（全経路で解放） | `511972e` | 実装 |
| **3 (D1)** | 自動双方向クリップボード同期（`clipboard_watch` + echo guard、receiver 送信チャネル + controller 適用） | `79ca759` | 実装（opt-in） |
| **4 (F2/G1/F1)** | 自動再接続（指数バックオフ）+ 接続受理トグル + ピア探索（ポートプローブ） | `6574277` | 実装 |
| **5 (B2)** | 名前付きスクリーン隣接グラフ `layout_graph`（純・テスト、双方向リンク + neighbor 解決） | `a0e74de` | **基盤のみ** |
| A2 | 全エッジ同時切替 | — | 5 の N-client ランタイムに従属（未） |

### 検証

- `cargo fmt` / `cargo check --workspace` / `cargo clippy --workspace`（tailkvm-win32 warning ゼロ） / `npm run build` 全 green。
- `cargo test --workspace`: ✅ 0 failed。win32 lib 30 件（screen_space 6 + SwitchGuard 4 + layout_graph 5 + 既存）+ net/ui/loopback。

### 残エピック（item 5 の本体 B1 — N-client ランタイム）

- 複数クライアント同時セッション管理（サーバ=複数接続、宛先/スクリーンID をプロトコルに追加）。
- `layout_graph` を使った「越えたエッジ → 隣接スクリーンへ送出切替」のランタイム配線。
- N スクリーン配置 GUI（Windows ディスプレイ設定風）への拡張 + 永続化（F3）。
- A2（全エッジ同時）は上記に内包。
- 規模大・実機検証必須のため本セッションでは基盤（データモデル）までを実装。

### 実機検証が必要（未検証・要 2 台）

- seamless 絶対モード（item1）: エッジ跨ぎの滑らかさ・任意点復帰・カーソル confine 解放。
- dwell/dead-corner（item2）の誤爆防止体感。
- 双方向クリップボード（item3）: 両方向の自動同期と echo ループしないこと。
- 自動再接続（item4）: 切断→指数バックオフ再接続、Disconnect で停止、accept トグル、Discover 一覧。

### 次の推奨タスク

- item 5 本体（B1 N-client ランタイム）の設計 → 小さく段階実装。
- 実機フィードバックに基づく seamless / dwell パラメータ調整、フェーズ3（IME composition）。

## Cycle 15 / N-client ランタイム（B1）設計 + B1.1〜B1.4 実装

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Plan + Feat（中核ランタイム、opt-in、純ロジックは全テスト）

`docs/multi-client-runtime-design.md` を作成（接続方向維持＝基本 N-client に wire 変更不要、
中核は `MultiScreenSpace` と `run_router`、段階計画 B1.1〜B1.7）。続けて中核 4 フェーズを実装。

| Phase | 内容 | commit |
| --- | --- | --- |
| 設計 | `docs/multi-client-runtime-design.md` | `f3a8884` |
| B1.1 | `MultiScreenSpace`（N 画面結合座標、純・テスト 4 件 + graph 5 件） | `bb10e29` |
| (掃除) | `Edge::from_str`→`from_label`（win32 clippy ゼロ） | `4c63a42` |
| B1.2 | 名前付き複数セッション（`sessions` map、`connect_screen`/`disconnect_screen`/`list_screens`、supervisor 共通化） | `afcefb6` |
| B1.3 | hook 転送を `SenderTarget{Fixed,Active}` 化（active 動的解決の土台） | `50ac8ec` |
| B1.4 | `run_router`（論理カーソル権威・local 追従/remote 絶対送出・hook の active 切替）+ start/stop コマンド + 右チェーン UI | `64550c3` |

### 検証

- `cargo fmt`/`check --workspace`/`clippy`（新規 warning なし、win32 ゼロ）/`test --workspace`（0 failed、
  win32 lib **38 件**: screen_space 6 + SwitchGuard 4 + layout_graph 5 + MultiScreenSpace 4 + 既存）/`npm run build` 全 green。

### 重要な未検証 / 残り

- **B1.4 は 3 台実機検証が必須**（純座標は検証済みだが、ルータの実遷移・hook active 切替・カーソル confine は実機要）。
- 残フェーズ **B1.5（クリップボード N ブロードキャスト）/ B1.6（N画面配置GUI+永続化 F3+起動時自動接続）/ B1.7（ScreenInfo 交換でリモート実サイズ補正・A3 統合）**。
- C1 の dwell/dead-corner を router のエッジ判定へ適用するのは後続改善（現状 router は即時切替）。
- 既知の課題: active session が再接続するとルータの active_slot が一時 stale（次 tick で MouseSetPosition は fresh 解決するため影響限定）。

### 次の推奨タスク

- B1.5 → B1.6 → B1.7 を順に。または 3 台実機検証のフィードバック反映。

## Cycle 16 / N-client ランタイム B1.5〜B1.7（完成）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Feat（N-client ランタイム残フェーズ）

| Phase | 内容 | commit |
| --- | --- | --- |
| B1.5 | クリップボード N ブロードキャスト（`broadcast_clipboard` で全セッション + 1:1 チャネルへ、echo guard 維持） | `c91fd61` |
| B1.6 | `SavedLayout` JSON 永続化（`%APPDATA%\TailKVM\layout.json`、`save_layout`/`load_layout`）+ 起動時自動接続（router は自動起動しない）+ JSON エディタ UI。`start_named_session` 抽出 | `bf8ca01` |
| B1.7 | `WireMessage::ScreenInfo` 追加。receiver が HelloAck 後に実仮想スクリーンサイズを報告 → controller が `screen_sizes` に記録 → router が実サイズで MultiScreenSpace を構築（A3 統合） | `67b959b` |

### 結果

- **N-client ランタイム（B1.1〜B1.7）は機能的に完成**。複数クライアント接続・名前付きセッション・
  論理カーソル権威ルータ・絶対送出・hook active 切替・クリップボード N 配信・レイアウト永続化/自動接続・
  実サイズ交換を実装。
- 検証: `cargo fmt`/`check --workspace`/`clippy`（新規 warning なし、残 6 は session1 既知 style lint）/
  `test --workspace`（0 failed、win32 lib 38・net 6+loopback2・ui 10・core1）/`npm run build` 全 green。

### 重要な未検証 / 残り（実機・将来）

- **3 台実機検証が必須**（座標/プロトコルは検証済み、ルータ実遷移・hook active 切替・confine・
  クリップボード N 配信・自動接続は実機要）。
- グラフィカル NxN 配置エディタ（現状 JSON 設定）、C1 dwell の router 適用、
  client→sibling クリップボード relay、ロック画面/UAC/モニタ hotplug のエッジケース。

### 次の推奨タスク

- 3 台実機検証 → フィードバック反映。新リリース（bobnote-4）作成も可。

## Cycle 17 / 将来改善 4 件（#1〜#4）

- 日付: 2026-06-02
- 担当: Claude (Opus 4.8)
- 種別: Feat（N-client ランタイムの後続改善）

| # | 内容 | commit |
| --- | --- | --- |
| #2 | router のエッジ判定に `SwitchGuard`（dwell + dead-corner）適用。誤爆防止 | `2dc3c9e` |
| #3 | client→sibling クリップボード relay（サーバ hub、`relay_clipboard`、origin 除外） | `ffd2d63` |
| #4 | モニタ hotplug 再同期（receiver が 5s ポーリングで ScreenInfo 再送）+ `docs/os-limitations.md`（secure desktop / UIPI / hotplug / clipboard / failsafe） | `c3fcfd1` |
| #1 | 視覚的レイアウトエディタ（左→右カード、add/reorder/remove、Apply=connect 全+router 起動、Save） | `974101e` |
| 掃除 | `run_controller_session` の `too_many_arguments` allow | （本コミット） |

### 検証

- `cargo fmt`/`check --workspace`/`clippy`（substantive warning は session1 既知の 6 件のみ）/
  `test --workspace`（0 failed）/`npm run build` 全 green。

### 残課題（将来 / 実機）

- 稼働中 router の `MultiScreenSpace` ライブ再構築（現状は再起動で反映）。
- per-monitor DPI 厳密マッピング、ロック検知の UI 表示、本格 2D ドラッグ配置エディタ。
- **3 台実機での全機能検証**（座標/プロトコルは検証済み、I/O・遷移・relay・hotplug は実機要）。

### 次の推奨タスク

- 3 台実機検証 → フィードバック反映、または新リリース bobnote-4 作成。
