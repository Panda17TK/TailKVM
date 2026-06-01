# TailKVM v0.1.0 — Bob-note verification build #2

`v0.1.0-bobnote-1` の後続プレリリース。実装精査で見つかった**安全バグの修正**と堅牢性・性能の改善を含む。
`claude/pdca-tailkvm-software-kvm` ブランチから生成。

## bobnote-1 からの主な変更

- **[安全・重要] failsafe ロックアウト修正**: 手動フックキャプチャ中に `Ctrl+Alt+Pause` / peer 切断が
  起きた際、低レベルフックが解除されずローカル入力が抑止されたままになる（ロックアウト）バグを修正。
  どの終了経路でもフックを確実にアンインストールするようにした。
- **[堅牢性] receiver 単一接続化（最新優先）**: 2 台目の controller 接続時に旧接続を協調的に閉じ、
  stuck 入力を解放。ゾンビ接続から再接続で自己回復。
- **[性能] TCP_NODELAY**: 入力イベントを即時送信（Nagle 無効化）。
- **[性能] フック転送のブロッキング受信化**: 5ms ポーリング廃止、イベント即時起床・wakeup 削減。
- **[性能] mouse move の UI 更新スパム削減**。
- **[機能] クリップボード（テキスト）共有**、**トレイ "Pause" を全停止キルスイッチ化**。
- **[PoC] Raw Input マウス診断（観測専用）** を追加（remote mode 未配線）。

## 品質ゲート（このビルド時点）

- `cargo fmt` / `cargo check --workspace` / `cargo clippy --workspace`（新規 warning なし） / `npm run build`: ✅
- `cargo test --workspace`: ✅ 0 failed（+ 実クリップボード FFI は `--ignored` で別途 pass）。

## 含まれる機能 / 検証手順 / 既知の制限

- bobnote-1 のリリースノート（`docs/release-notes-v0.1.0-bobnote.md`）を参照。
- 単体マシン検証: `docs/single-machine-testing.md`。2 台検証: `TASK_LOG.md` 各タスク。

## 特に実機確認してほしい点

1. **failsafe**: 手動キャプチャ中の Ctrl+Alt+Pause / 切断でローカル入力が即復帰すること（今回の修正点）。
2. receiver 単一接続（最新優先）の挙動。
3. クリップボード送受信・トレイ Pause。

> プレリリース。実運用前に実機検証を完了してください。
