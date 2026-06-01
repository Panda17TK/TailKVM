# 設計メモ: IME / 半角全角 / JIS-US 差分の扱い (Task 9D)

- 日付: 2026-06-01
- ステータス: Draft（設計のみ。実装は別タスク）
- 関連: Task 9B-1/9B-2（keyboard capture）、Task 9C（keyboard layout foundation）

本メモは、controller（操作元）と receiver（操作先）でキーボード環境が異なる場合に
正しく文字入力を再現するための設計指針をまとめる。現状実装の確認、問題の整理、
選択肢、推奨アプローチ、未解決事項を扱う。

---

## 1. 現状実装（2026-06-01 時点）

キーボード転送は `WireMessage` の 2 種で行われる（`crates/tailkvm-net/src/protocol.rs`）:

- `KeyboardKey { vk: u16, scan_code: u16, down: bool, extended: bool }`
  - フック (`keyboard_hook.rs`, `WH_KEYBOARD_LL`) が拾った生イベントをそのまま転送。
  - receiver は `keyboard::send_key_event` で `SendInput` 注入。
    `scan_code != 0` なら `KEYEVENTF_SCANCODE`、無ければ `vk`。`extended` フラグも伝播。
- `KeyboardText { text: String }`
  - UTF-16 + `KEYEVENTF_UNICODE` で文字そのものを注入（レイアウト非依存）。

レイアウト識別は Task 9C で `keyboard_layout::current_keyboard_layout()` が
入力ロケール (HKL) と物理キーボード種別 (`GetKeyboardType`) を取得できる。

### 現状の含意

- `KeyboardKey` 経路は **「物理キー（スキャンコード）」と「VK」を両方送っている**が、
  receiver 側は scan_code 優先で注入する。スキャンコードは物理キー位置であり、
  receiver 側の**ソフトレイアウト (HKL) で文字へ写像される**。
- したがって controller と receiver の HKL / 物理キーボードが異なると、
  同じ物理キーでも別の文字になる、または存在しないキーになる。

---

## 2. 差分の 3 軸

キーボード差分は独立した 3 つの軸に分解できる。混同すると設計を誤る。

### 軸 A: 物理キーボード（JIS vs US など）

- JIS と US では**物理キーの集合と配置が異なる**。
  - JIS のみ存在: `半角/全角`、`変換`、`無変換`、`カタカナ/ひらがな`、`¥`(0x7D)、`ろ`(0x73)。
  - 記号位置が異なる: `@ [ ] : ] \\` 等。`"` `&` `'` `(` `)` 等の shift 面も違う。
  - US 102 と JIS 106/109 でスキャンコードのセットが一部異なる。
- 検出: `GetKeyboardType(0)`（7 = 日本語）, subtype。Task 9C で取得済み。

### 軸 B: 入力ロケール / ソフトレイアウト (HKL)

- スキャンコード ⇔ VK ⇔ 文字 の写像テーブル。`GetKeyboardLayout` の HKL。
- 同じ物理 US キーボードでも、JP レイアウトを選べば JIS 記号配置になる（逆も同様）。
- `MapVirtualKeyEx` / `ToUnicodeEx` は HKL を引数に取り、特定レイアウトでの写像を計算できる。

### 軸 C: IME 状態（半角全角・変換モード）

- IME ON/OFF、かな/ローマ字入力、変換モード（ひらがな/全角カナ/半角カナ/全角英数）。
- **HKL にも GetKeyboardType にも含まれない**。IME プロセス（例: MS-IME）の内部状態。
- `半角/全角` キー (VK_KANJI/VK_DBE_*) は IME の ON/OFF をトグルする特殊キー。
- controller 側でフックが拾ったキーをそのまま送ると、receiver 側 IME の状態に依存して
  全く違う結果になる（例: controller は IME OFF のつもりでも receiver が IME ON）。

---

## 3. 各軸が引き起こす問題

| シナリオ | 経路 | 問題 |
| --- | --- | --- |
| US controller → JIS receiver、英数のみ | KeyboardKey(scan) | scan が receiver の JP レイアウトで別記号になる可能性（特に記号キー）。 |
| JIS controller → US receiver | KeyboardKey(scan) | `¥`/`変換` 等のスキャンコードが US レイアウトに存在せず無視/誤変換。 |
| 日本語入力（かな漢字変換） | KeyboardKey | IME 状態が両端で食い違い、ローマ字列が化ける。確定済みテキストすら崩れる。 |
| 絵文字・確定済み文字列の貼り付け的入力 | KeyboardText | レイアウト非依存で安全。ただしキーリピート/修飾キー併用や IME 未確定は表現不可。 |

要点: **scan_code 経路は「物理キーの再現」、Unicode 経路は「文字の再現」**で、
目的が異なる。日本語/IME を絡めると物理キー再現は破綻しやすい。

---

## 4. 設計の選択肢

### 経路の使い分け（誰が文字を確定するか）

1. **物理キー再現（現状の KeyboardKey/scan_code 経路）**
   - 長所: 低レイテンシ、ゲーム/ショートカット/修飾キー併用に強い、IME を receiver 側で使える。
   - 短所: 両端の物理＋ソフトレイアウトが一致している前提。JIS/US 混在で破綻。
2. **VK 経路（scan を捨て VK 主体で送る）**
   - receiver 側で `vk` から `SendInput`（scancode 無し）。レイアウト非依存性がやや上がるが、
     VK→文字も結局 receiver の HKL に依存し、記号は完全には解決しない。
3. **文字確定経路（controller 側で文字を解決して KeyboardText 送出）**
   - controller 側で `ToUnicodeEx(vk, scan, keystate, hkl)` を使い、押下時点の**文字列**へ解決し、
     `KeyboardText`（Unicode 注入）で送る。
   - 長所: receiver のレイアウト/IME に一切依存せず確実。日本語確定済みテキストも安全。
   - 短所: 修飾キー単体・ショートカット（Ctrl+C 等）・キーリピート・ゲーム入力・
     IME の未確定変換中の挙動は表現できない。Dead key / 合成も要考慮。

### IME の扱い

- **方針 I（推奨の初期方針）: IME は controller 側で完結させる。**
  - controller でローカル IME を使って**確定した文字**を `KeyboardText` で送る。
  - receiver 側は IME OFF（直接入力）を前提に Unicode 注入。
  - controller 側フックは IME に渡る前/後どちらでキーを拾うか要検証
    （`WH_KEYBOARD_LL` は IME 変換前の生キーを拾うため、確定文字を得るには別経路が必要、後述）。
- 方針 II: receiver 側 IME を使う（生キー転送）。両端の IME 設定一致が前提で脆い。非推奨。

---

## 5. 推奨アプローチ（段階導入）

レイテンシ重視のショートカット系と、確実性重視のテキスト入力を**ハイブリッド**で扱う。

### フェーズ 1: レイアウト一致検出と警告（最小）

- 接続時に controller / receiver の `KeyboardLayoutInfo`（Task 9C）を交換する
  `WireMessage::KeyboardLayout { language_id, keyboard_type, ... }` を追加。
- 不一致（HKL もしくは keyboard_type が異なる）を UI に警告表示。
- 既存の scan_code 経路はそのまま（一致環境では正しく動く）。

### フェーズ 2: 修飾＋非文字キーは物理経路、文字は Unicode 経路に分離

- controller 側で、押下キーが「文字を生成するキー」か「制御/修飾/ファンクション」かを判定。
  - 制御系（Ctrl/Alt/Win 併用、矢印、F キー、Enter/Tab/Esc/Backspace 等）→ `KeyboardKey`。
  - 文字生成キー → `ToUnicodeEx` で controller の HKL に基づき文字へ解決 → `KeyboardText`。
- これにより JIS/US 差分の大半（記号位置ずれ）を receiver 非依存で吸収。

### フェーズ 3: IME 完結（日本語入力）

- 確定文字の取得方法を決める。候補:
  - (a) `WH_KEYBOARD_LL` は変換前の生キーのため、確定文字は拾えない。
    代わりに controller 側に**不可視の入力先**（隠しウィンドウ + IME）を用意し、
    `WM_IME_COMPOSITION`/`WM_CHAR` で確定文字列を取得して `KeyboardText` 送出する設計を検討。
  - (b) もしくは controller 側はローカルアプリにそのまま入力させ、クリップボード/UI Automation
    ではなく専用の入力キャプチャを使う。← 複雑度高、要 PoC。
- 半角/全角キー (IME トグル) はローカル controller 側のみで作用させ、receiver には送らない。

### フェーズ 4: 物理キー remap テーブル（任意）

- どうしても物理経路が必要なケース（ゲーム等）向けに、
  JIS↔US のスキャンコード/VK 変換テーブルを `keyboard_layout` に追加。
- 送信側 or 受信側どちらで変換するか、レイアウト情報交換（フェーズ1）を前提に決定。

---

## 6. 関係する Win32 API メモ

- `GetKeyboardLayout(threadId)` → HKL（Task 9C 実装済み）。
- `GetKeyboardType(0|1|2)` → 物理種別/サブタイプ/F キー数（Task 9C 実装済み）。
- `GetKeyboardState` / `ToUnicodeEx(vk, scan, keyState, buf, len, flags, hkl)`
  → 指定レイアウトでの文字解決。dead key の状態を持つ点に注意。
- `MapVirtualKeyEx(code, mapType, hkl)` → VK⇔scan⇔char の相互変換。
- `VkKeyScanEx(ch, hkl)` → 文字から VK + 修飾を逆引き（receiver 側で文字を物理キーへ戻す場合）。
- IME: `ImmGetConversionStatus` / `ImmSetConversionStatus`、`WM_IME_*`、
  VK_KANJI(0x19), VK_CONVERT(0x1C), VK_NONCONVERT(0x1D), VK_DBE_* 系。

---

## 7. 未解決事項 / 要 PoC

1. `WH_KEYBOARD_LL` で確定文字を得られないため、IME 完結（フェーズ3）の確定文字取得方法。
2. `ToUnicodeEx` の dead key / 合成（例: アクセント記号）でのステート管理。
3. キーリピート（オートリピート）を Unicode 経路でどう扱うか。
4. ショートカット（Ctrl+記号など、記号位置がレイアウト依存）の判定境界。
5. フェーズ1 のレイアウト情報交換を `Hello`/`HelloAck` に載せるか、独立メッセージにするか。
6. 実機（JIS receiver / US controller の両方向）での実測データ収集（Task 9C の UI で取得可能）。

---

## 8. 次アクション

- [x] フェーズ 1（レイアウト情報交換 + 不一致警告）— 実装済み（Task 9D phase 1）。
- [ ] JIS receiver / US controller 双方向で Task 9C の `Check keyboard layout` を実測し、
      `language_id` / `keyboard_type` の実値を本メモに追記。
- [x] `ToUnicodeEx` ベースの文字解決 + キー分類（フェーズ2 のコア）を実装済み（下記 §9）。

---

## 9. 実装状況（2026-06-02 更新）

### 実装済み（opt-in、既定 OFF）

`crates/tailkvm-win32/src/key_class.rs`（純ロジック・ユニットテスト済み）と
`keyboard::resolve_key_text`（`ToUnicodeEx`）を追加し、キーボード転送に
**character-resolution モード**を opt-in で配線（UI チェックボックス「Resolve characters (JIS/US bridge)」、
`set_resolve_characters` コマンド、転送ループが live に参照）。

ON 時のルーティング（`classify_key`）:

| キー種別 | ルート | 挙動 |
| --- | --- | --- |
| 修飾キー自体（Ctrl/Shift/Alt/Win） | Physical | `KeyboardKey` 転送（combo 用に保持） |
| Ctrl/Alt/Win 併用（Ctrl+C, **Win+X**, **Alt+Tab** 等） | Physical | scan/vk 転送 |
| 制御/ナビ/ファンクション（Enter/Tab/Esc/Space/矢印/F キー等） | Physical | scan/vk 転送 |
| 印字キー（英字/記号、Shift 込み） | Character | controller レイアウトで `ToUnicodeEx` 解決 → `KeyboardText`（Unicode）。**JIS/US 記号位置差を吸収** |
| IME トグル/変換（**半角/全角**・変換・無変換・かな・Kanji） | ImeLocal | **転送しない**（receiver IME を反転させない） |

- dead key / 未解決は physical 経路へフォールバック（stuck 解放のため tracking）。
- これにより **Win / Alt+Tab**（physical 経路で従来どおり）、**半角/全角**（drop で receiver IME 非干渉）、
  **JIS/US 記号差**（Unicode 解決で吸収）、**IME OFF 直接入力の英数記号**が opt-in で扱える。

### 既定 OFF 時

従来どおり全キーを scan/vk の `KeyboardKey` で転送（レイアウト一致環境向け、挙動不変）。

### 未実装（フェーズ 3 / 要 2 台・実機検証）

- **かな漢字 IME 変換（未確定 composition）の取り込み**は未実装。`ToUnicodeEx` は単一キーの
  レイアウト文字のみ解決し、IME の変換結果（確定文字列）は取得しない。これには controller 側に
  隠しウィンドウ + IME で `WM_IME_COMPOSITION`/`WM_CHAR` を拾う PoC（§5 フェーズ3）が必要。
- CapsLock 状態は簡易化のため未考慮（Shift のみ反映）。英字大文字は Shift 押下で対応。
- 上記ルーティングの実機挙動（特に Shift+Unicode 同時、ショートカット境界）は **2 台での実測が必要**。
