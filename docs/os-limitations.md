# OS 制約とエッジケース（Windows）

TailKVM が Windows の仕様上できないこと・注意点と、現状の扱い。

## 1. セキュアデスクトップ（ロック画面 / UAC / Ctrl+Alt+Del）

- **`SendInput` はセキュアデスクトップに届かない。** ロック画面、UAC 昇格ダイアログ、
  `Ctrl+Alt+Del` 画面（Winlogon デスクトップ）には、通常ユーザー権限のプロセスから入力注入できない。
  - receiver がロック中/UAC 表示中は、controller からのマウス・キーボードが効かない。
  - これは OS のセキュリティ境界であり回避不可（管理者権限でも secure desktop 越えは不可）。
- **`Ctrl+Alt+Del` 自体を送ることはできない**（SAS は物理キーボード/特権パスのみ）。対象外。
- **低レベルフック（`WH_KEYBOARD_LL`/`WH_MOUSE_LL`）もセキュアデスクトップ遷移中は無効化される**。
  → ロック/UAC 中はキャプチャ・注入とも停止する前提。UI の `last_event` で状況が分かるようにする。
- 現状の扱い: 制限として明記。将来、ロック検知（`WTSRegisterSessionNotification` 等）で
  UI に「receiver is locked」を表示する案はあるが未実装。

## 2. 管理者権限アプリ（UIPI / インテグリティレベル）

- 通常権限の TailKVM から、**より高いインテグリティレベル（管理者昇格）ウィンドウへは入力注入できない**
  （UIPI: User Interface Privilege Isolation）。`SendInput` が無視される（戻り値 0）。
  - 管理者として実行中のアプリ（例: 管理者 PowerShell）を操作するには、TailKVM 自体を管理者で実行する必要がある。
- 現状の扱い: インストーラは通常権限で動作。必要なら「管理者として実行」を案内（Firewall 設定は別途昇格 PowerShell）。

## 3. モニタ hotplug / 解像度・DPI 変更

- 稼働中にモニタ抜き差し・解像度変更が起きると、receiver の仮想スクリーンサイズが変わる。
- **現状の扱い（実装済み）**: receiver は接続中、5 秒間隔で `get_monitor_topology` をポーリングし、
  仮想スクリーンサイズが変化したら `ScreenInfo` を再送する。controller の router は最新サイズで
  座標マッピングを更新できる（次回 router 起動時に反映。稼働中 router の即時再構築は未対応）。
- 残課題: 稼働中 router の `MultiScreenSpace` をライブ再構築する（現状は再起動で反映）。
  per-monitor DPI 差の厳密なマッピング（仮想スクリーン全体ではなく各モニタ単位）。

## 4. その他

- **クリップボード**: テキストのみ（CF_UNICODETEXT）。画像/HTML/ファイル（CF_DIB/CF_HTML/CF_HDROP）は未対応。
  クリップボードを他プロセスが掴んでいると `OpenClipboard` が一時的に失敗しうる（リトライで解消）。
- **ゲーム等の Raw Input 専用アプリ**: receiver 側で `SendInput` の合成入力を受け付けないゲームがある
  （anti-cheat 含む）。物理デバイス前提のアプリは対象外になりうる。
- **複数ユーザー / RDP セッション**: 別セッション（RDP/切替ユーザー）への注入は想定外。

## 5. フェイルセーフ（再掲）

- `Ctrl+Alt+Pause` はすべてのキャプチャ（mouse/keyboard hook・mouse-move・router）を停止し、
  フックをアンインストールしてローカル入力を回復する。セキュアデスクトップ中はフック自体が無効化されるため、
  ロック解除後に通常動作へ戻る。
