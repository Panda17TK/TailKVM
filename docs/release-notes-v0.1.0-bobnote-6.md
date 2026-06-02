# TailKVM v0.1.0 — Bob-note verification build #6（接続診断 + クイックスタート）

実機ログで判明した「TCP session error（＝接続失敗）」を分かりやすくするための診断改善版。
bobnote-5 のクイックスタートに加え、接続できない原因をその場で表示します。

## 重要：接続の仕組み
- 「操作する側」が「操作される側」へ TCP 接続します。
- したがって **操作される側（相手PC）で `Start receiver` を押し、`Install firewall rule` を実行**しておく必要があります。
- 入れる IP は **相手PCの Tailscale IP**（自分のIPではない）。

## このビルドの改善
- クイックスタートに **接続できない時のチェックリスト**を表示。
- 未接続時に **実際の理由**（connection refused / timeout 等）を表示（従来は「TCP session error」のみ）。
- **このPCの Tailscale IP** を表示（相手側で入力する値）。

## 最短手順
1. 両PCで TailKVM を起動。
2. 相手PC: 下部「TCP Session」→ **Start receiver**、続けて **Install firewall rule**。
3. メインPC: クイックスタートに相手の Tailscale IP を入れて **① 接続**（接続中表示になればOK）。
4. **② マウス共有を開始（ミラー）** → マウスを動かすと相手のカーソルが動く。**停止**で戻る。

## 品質ゲート
- cargo test --workspace（57 passed）/ npm run build / tauri build すべて成功。
