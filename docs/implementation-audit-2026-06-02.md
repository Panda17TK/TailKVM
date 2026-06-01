# 実装精査（Audit）2026-06-02

Session 1–2 で実装した内容を精査し、簡易実装・課題・パフォーマンス/見た目/動作への影響を洗い出し、
打ち手を設計・実施した記録。対象は `apps/tailkvm-ui/src-tauri/src/lib.rs` 中心、関連クレートを含む。

凡例: 重大度 H/M/L、状態 ✅=本セッションで対応 / 📝=記録（フォロー候補）。

## パフォーマンス

### A1 (H) ✅ TCP_NODELAY 未設定 → 設定済み

- 症状: controller の `TcpStream::connect` / receiver の `accept` で Nagle が有効のまま。
  小さな JSON 行（mouse move / key event）がバッファ・coalesce され、KVM の入力追従にレイテンシ。
- 打ち手: 両ソケットで `set_nodelay(true)`（best-effort）。commit `6df5be6`。
- 効果: 単発入力イベントが即時送信。MWB より安定という目標に直結。

### A2 (M) ✅ 高頻度 update_tcp_state + Debug format スパム → 削減

- 症状: controller outbound 分岐が **MouseMove 含む全送信**で `format!("...{outbound:?}")` をロック下実行
  （remote 中 ~30/s）。アロケーション/ロック競合 + capture ループの要約 last_event を上書きしちらつき。
- 打ち手: outbound で `MouseMove` のときは `update_tcp_state` をスキップ（capture ループが throttle 済み要約を出す）。
  commit `6df5be6`。
- 補足: hook 転送ループ（クリック/ホイール/キー）は **低頻度**（人間操作で数十/s 上限）かつ単発クリックの
  フィードバックが有用なため per-event 更新は維持（過剰 throttle で click/key の可視性を落とさない判断）。

### A3 (M) 📝 mouse-move が GetCursorPos/SetCursorPos ポーリング（33ms）

- ちらつき・warp 誤検出・量子化・ポインタ加速二重適用。→ `docs/raw-input-mouse-design.md`（フェーズ A PoC）で対応予定。
- 本監査では対象外（設計済み、別タスク）。

### A4 (L) 📝 hook の std::mpsc → 5ms ポーリングブリッジ

- フックスレッド（同期）→ async タスクへ `try_recv` + `sleep(5ms)`。最大 5ms のレイテンシ。
- 許容範囲。将来 tokio mpsc + blocking 送信へ寄せると低減可能（フォロー候補）。

## 動作 / 正しさ

### B1 (M) 📝 receiver が複数接続を受理

- `start_tcp_receiver` の accept ループは接続ごとに `handle_receiver_stream` を spawn。
  2 つの controller が同時接続すると両方が注入し snapshot を奪い合う。
- Tailscale + 信頼前提で実害は低いが堅牢性の穴。打ち手案: 接続中フラグ（`AtomicBool`）で
  2 本目以降を `Disconnect{reason:"busy"}` で拒否、または最新接続を優先し旧接続を閉じる。
- 本監査では未実装（要件確定後に小さく実装）。

### B2 ✅(確認のみ) heartbeat / return-edge / clipboard は正常

- controller→receiver heartbeat 2s（`time::interval`、missed tick=Delay）。HeartbeatAck で `last_heartbeat_ms` 更新。
- return-to-local は receiver の `MousePosition` 応答 → controller が `is_remote_return_edge` 判定で
  `capture_running=false`。配線済み・ユニットテスト済み（dead code ではない）。
- clipboard は controller→receiver 一方向・手動 + echo guard（設計どおり）。

## 見た目 / UI

### C1 (L) 📝 clipboard ボタンの配置

- "Send clipboard to peer" がキーボード操作群末尾に追加され、グルーピングがやや雑（機能影響なし）。
  将来 UI 整理時に「クリップボード」セクション化を検討。

### C2 ✅ last_event のちらつき

- A2 の outbound MouseMove スキップで、remote 中に capture ループの要約が安定表示されるよう改善。

### C3 ✅(確認のみ) UI ポーリング 2s

- `setInterval(refreshTcpSession, 2000)`。妥当。負荷・ちらつき問題なし。

## 簡易実装 / プレースホルダ

### D1 (L) ✅ get_app_status の古い文字列

- `"...Task 5 OK."` → `format!("TailKVM v{} backend running.", CARGO_PKG_VERSION)`。commit `eb826b6`。

### D2 (M) ✅ トレイ "Pause input forwarding" 未実装 → 配線

- 旧: `println!("... not implemented yet.")` のみ。
- 打ち手: `stop_mouse_capture` の停止処理を `pause_all_capture(&AppState)` に共通化し、
  トレイ "pause" から `app.state::<AppState>()` 経由で呼ぶ。全キャプチャ停止 + stuck 解放を行う
  **手動キルスイッチ**として機能（Ctrl+Alt+Pause failsafe を補完、置換しない）。commit `eb826b6`。

### D3 (L) 📝 tailkvm-core::add 未使用プレースホルダ

- `add()` + placeholder test のみ。実害なし。将来 core ロジック（座標変換等）の置き場として活用 or 削除。

## 安全性の不変条件（精査で確認）

- Ctrl+Alt+Pause failsafe の二重経路（keyboard hook proc + mouse loop ポーリング）は **無変更**。
- firewall RemoteAddress 既定 `100.64.0.0/10`（Tailscale CGNAT）も無変更。
- 今回のリファクタは latency / UI 更新頻度 / 停止経路の共通化のみで、抑止(`return 1`)・注入・解放ロジックは不変。

## 本監査での commit

| commit | 内容 |
| --- | --- |
| `6df5be6` | perf: TCP_NODELAY + MouseMove UI 更新スパム削減（A1/A2） |
| `eb826b6` | refactor: pause_all_capture 共通化 + tray Pause 配線 + get_app_status 修正（D1/D2） |

## 残フォロー（優先度順）

1. B1 receiver 単一接続化（堅牢性）。
2. A3 Raw Input フェーズ A PoC（ちらつき/精度）。
3. A4 hook→async を tokio チャネル化（レイテンシ微減）。
4. C1 UI セクション整理 / D3 core クレート整理。
