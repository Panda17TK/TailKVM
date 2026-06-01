# Raw Input マウスキャプチャ 設計メモ（Task 10）

状態: 調査 / 設計（コード変更なし）。実装する場合は小さな PoC に限定し、既存 remote mode を壊さない。

## 1. 背景 — 現行方式と問題

現行の remote mode マウス移動は **`GetCursorPos` + `SetCursorPos` の "cursor lock warp"** 方式
（`apps/tailkvm-ui/src-tauri/src/lib.rs` の `start_mouse_capture` ループ）:

1. 仮想スクリーン中央 `(lock_x, lock_y)` を毎フレームの基準点とする。
2. `GetCursorPos` で現在位置を読み、`dx = cur.x - lock_x` を相対量とする。
3. 直後に `SetCursorPos(lock_x, lock_y)` でカーソルを中央へ戻す（warp）。
4. `dx,dy` に gain を掛け clamp し、`MouseMove` として送出。

### 既知の問題（この方式に内在）

| 問題 | 原因 |
| --- | --- |
| ちらつき / カーソルが一瞬見える | 毎フレーム実際のカーソルを動かして戻すため、warp 残像が出る。 |
| warp フィードバック誤検出 | 自分の `SetCursorPos` が次フレームの `GetCursorPos` に混ざる。`ignored_warp_frames` と `warp_threshold = max(max_delta*8, 800)` のヒューリスティックで弾いているが完全ではない。 |
| ポーリング間隔依存の精度 | `interval_ms`（既定 33ms）で離散サンプリング。速い動きで取りこぼし・量子化。 |
| ポインタ加速 / DPI の二重適用 | OS のポインタ加速・DPI スケールが乗った「画面座標差分」を送るため、receiver 側で再度補正が要る。 |
| マルチモニタ境界での跳ね | 仮想スクリーン端や DPI 境界をまたぐと `GetCursorPos` が不連続になりうる。 |

## 2. Raw Input（WM_INPUT）方式

`RegisterRawInputDevices` で HID マウスを購読し、`WM_INPUT` で **HID 生の相対移動量
(`RAWMOUSE.lLastX/lLastY`)** を直接受け取る。OS のポインタ加速・クリッピング・DPI スケールが
**乗る前** のデバイス相対量なので、KVM の「相対移動を相手へ流す」用途に理想的。

### 関連 API / 構造体

- `RegisterRawInputDevices(*const RAWINPUTDEVICE, count, cbSize)` — `usUsagePage=0x01, usUsage=0x02`（マウス）。
- `GetRawInputData(HRAWINPUT, RID_INPUT, ...)` — `RAWINPUT` を取り出す。
- `RAWMOUSE`: `usFlags`（`MOUSE_MOVE_RELATIVE`=0 が通常マウス / `MOUSE_MOVE_ABSOLUTE` はタブレット等）、
  `lLastX`,`lLastY`（相対量）、`usButtonFlags`（ボタン/ホイール）、`usButtonData`（ホイール delta）。
- フラグ:
  - `RIDEV_INPUTSINK` — フォアグラウンドでなくても入力を受ける（hidden window 必須）。
  - `RIDEV_NOLEGACY` — レガシーな `WM_MOUSEMOVE`/カーソル移動を抑止（＝ローカルカーソルが動かない）。
  - `RIDEV_REMOVE` — 購読解除。

### 必要な windows-sys features（実装時）

`Win32_UI_Input`（`RegisterRawInputDevices` / `GetRawInputData` / `RAWINPUT*` / `RID_INPUT` /
`RAWINPUTDEVICE` / `RIDEV_*`）。hidden message window 用に `Win32_UI_WindowsAndMessaging`（既に有効）。

## 3. 役割分担（SendInput / low-level hook / Raw Input）

| 機構 | 役割 | TailKVM での位置づけ |
| --- | --- | --- |
| `SendInput` | 入力**注入** | receiver 側で確定（変更なし）。 |
| `WH_MOUSE_LL` / `WH_KEYBOARD_LL`（low-level hook） | ボタン/ホイール/キーの**捕捉 + ローカル抑止** | 現行の click/wheel/keyboard 捕捉（維持）。`return 1` で抑止できるのが強み。 |
| Raw Input（WM_INPUT） | 高精度な**相対移動量取得** | **移動(dx,dy)のみ** を置き換える候補。ボタン/ホイールは引き続き hook で抑止する方が安全。 |

> 重要: Raw Input は「観測」専用で、`WH_MOUSE_LL` のような**ローカル抑止能力を持たない**
> （`RIDEV_NOLEGACY` を使えばカーソル移動自体は止まるが、これはシステム全体の挙動を変える劇薬）。
> したがって移動の抑止はこれまで通り SetCursorPos lock か NOLEGACY で実現する設計判断が要る。

## 4. 推奨アプローチ（段階導入・小 PoC）

### フェーズ A（PoC・低リスク・推奨）: 「lock 維持 + 相対量だけ Raw Input に置換」

- 既存の cursor lock（毎フレーム `SetCursorPos(lock_x, lock_y)`）は**残す**（カーソルを画面端に貼り付けて動かさない）。
- ただし送出する `dx,dy` は `GetCursorPos` 差分ではなく **WM_INPUT の `lLastX/lLastY` の累積**から取る。
- 利点: warp フィードバック誤検出・ポインタ加速の二重適用が消え、`ignored_warp_frames` /
  `warp_threshold` ヒューリスティックを撤去できる。リスクは小（NOLEGACY を使わないのでシステム挙動不変）。
- 実装: 新モジュール `crates/tailkvm-win32/src/raw_input_mouse.rs`。専用スレッドで hidden message window
  を作り（`CreateWindowExW` の message-only window: 親 `HWND_MESSAGE`）、`RegisterRawInputDevices`
  (`RIDEV_INPUTSINK`) → メッセージループで `WM_INPUT` を受け、`(dx,dy)` を mpsc 送信。停止時に
  `RIDEV_REMOVE` で購読解除し window 破棄（既存 hook モジュールと同じ Handle/Drop パターン）。
- 既存 remote mode ループは「`GetCursorPos` 差分」から「raw delta チャネル受信」へ差し替えるだけ。
  edge 検出（remote 開始前）は引き続き `GetCursorPos`（絶対位置が必要なので妥当）。

### フェーズ B（任意・高リスク）: `RIDEV_NOLEGACY` で warp 自体を廃止

- `RIDEV_NOLEGACY` でカーソルを物理的に固定し、`SetCursorPos` ループを撤去。ちらつき完全解消。
- リスク: NOLEGACY はシステム全体のマウス挙動を止めるため、**停止漏れ＝ローカルマウス不能**。
  必ず Ctrl+Alt+Pause failsafe で `RIDEV_REMOVE` を確実に呼ぶ二重化が前提。フェーズ A の安定後のみ検討。

## 5. failsafe への影響（厳守）

- `Ctrl+Alt+Pause` failsafe は**不変で維持**。フェーズ A では `GetAsyncKeyState` ポーリング
  （`cursor::is_ctrl_alt_pause_pressed`）がそのまま機能（カーソルは lock されるがキー状態は読める）。
- フェーズ B（NOLEGACY）採用時は、停止経路（manual stop / return edge / failsafe / peer 切断）すべてで
  `RIDEV_REMOVE` を呼ぶことを単体テスト＋実機で保証してから入れる。Raw Input window スレッドの Drop で
  必ず unregister する RAII を必須とする。

## 6. PoC のスコープと受け入れ条件

- スコープ: `raw_input_mouse.rs`（hidden window + WM_INPUT → mpsc<(i32,i32)>）の追加と、
  remote mode の移動量算出のみ差し替え（フェーズ A）。ボタン/ホイール/キーボードは現状維持。
- 既存 remote mode を壊さない: フラグ（例: `use_raw_input: Option<bool>`）で従来方式と切替可能にし、
  既定は従来方式のまま PoC を opt-in にする。
- 自動検証可能な部分: `RAWMOUSE` フラグ判定（`MOUSE_MOVE_RELATIVE` のみ採用、ABSOLUTE は無視）や
  delta 累積ロジックを純関数に切り出してユニットテスト。WM_INPUT 実受信は実機検証。
- 実機検証: ローカルカーソルが lock されたまま receiver が滑らかに動くこと、停止で完全復帰すること、
  Ctrl+Alt+Pause で即時停止すること。

## 7. 未解決事項 / 次アクション

1. message-only window をフック用スレッドと同居させるか専用スレッドにするか（専用推奨、既存パターン踏襲）。
2. `lLastX/lLastY` の `MOUSE_MOVE_ABSOLUTE`（RDP/タブレット/一部 VM）ケースの扱い（無視 or 絶対→相対変換）。
3. 高 DPI/高ポーリングレート（1000Hz）マウスでの mpsc バックプレッシャ（バッチ集約して送る）。
4. フェーズ A の gain/clamp は raw delta に対して再チューニングが必要（画面差分前提の現行既定値は過大かも）。

> 結論: **フェーズ A（lock 維持 + Raw Input 相対量）を小 PoC として opt-in 実装するのが最小リスク**。
> NOLEGACY によるちらつき完全解消（フェーズ B）は failsafe の停止保証を固めてから。
