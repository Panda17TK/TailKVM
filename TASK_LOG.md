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

- commit hash: （本コミットで記録）
- push: claude/pdca-tailkvm-software-kvm へ push（main へは push しない）

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
