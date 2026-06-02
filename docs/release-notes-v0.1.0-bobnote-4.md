# TailKVM v0.1.0 — Bob-note verification build #4

`v0.1.0-bobnote-3` の後続プレリリース。N-client ランタイムと全後続改善、既知課題 4 点の対応を含む
**全機能版**。`claude/pdca-tailkvm-software-kvm` ブランチから生成。

## bobnote-3 からの主な追加・改善

### マルチスクリーン（N-client）ランタイム
- 複数 Client への名前付き同時セッション + 自動再接続（指数バックオフ）。
- **論理カーソル権威ルータ**：`MultiScreenSpace` + レイアウトグラフでエッジ越えを解決し、
  active 画面へ絶対 `MouseSetPosition`。remote→remote は active 切替のみでフック再装着なし。
- **稼働中のライブ再構成**（router 再起動不要）：トポロジ再取得で `MultiScreenSpace` を atomic swap。
- クリップボードの **N ブロードキャスト + client→sibling relay**（サーバ hub）。
- `ScreenInfo` 交換でリモート実仮想スクリーンサイズを反映。
- レイアウト JSON 永続化 + 起動時自動接続。

### 座標・DPI
- **Per-Monitor-V2 DPI awareness を起動時に保証**（GetCursorPos/SetCursorPos/モニタ矩形/SendInput を
  physical-px 仮想デスクトップ空間に統一）。負原点・縦置き・解像度/DPI 差・上下左右配置に対応。

### UX / 状態表示
- **ロック検知**（local の secure desktop / lock を 2s ポーリング表示）。
- 画面ごとの接続状態（active / reconnecting）表示、ピア探索、接続受理トグル、Disconnect。
- **2D ドラッグ配置エディタ**（端末矩形をドラッグ、隣接からリンク推論、Apply で live 反映）+ 左→右簡易エディタ + JSON エディタ。
- トレイ "Pause" が全停止キルスイッチ。

### 入力
- マウス: 移動（相対 warp / Raw Input / seamless 絶対）/ クリック / ダブルクリック / 右 / 中 / X1・X2 / ホイール / ドラッグ。
- キーボード: テキスト注入 / 単発キー / フックキャプチャ / **文字解決モード**（`ToUnicodeEx` で JIS/US 記号差吸収、Win/Alt+Tab は物理、半角全角は drop）。
- Ctrl+Alt+Pause フェイルセーフ（二重化、全フック確実解除）。受信側 stuck key/button 解放。

## 品質ゲート（このビルド時点）

- `cargo fmt` / `cargo check --workspace` / `cargo clippy`（新規 warning なし） / `npm run build`: ✅
- `cargo test --workspace`: ✅ **57 passed; 0 failed; 1 ignored**（実クリップボード FFI は `--ignored` で別途確認）。

## 設定・ドキュメント

- OS 制約: `docs/os-limitations.md`（secure desktop / UIPI / hotplug / clipboard / failsafe）。
- N-client 設計: `docs/multi-client-runtime-design.md`。座標/キーボード設計: 各 docs/。

## 特に実機（2〜3 台）で確認してほしい点

1. DPI/解像度の異なるモニタ跨ぎでカーソルが破綻せず滑らかに遷移し、任意点で復帰すること。
2. router 稼働中に 2D エディタ「Apply live」/「Reconfigure live」で再起動なく配置が反映されること。
3. ロック表示（🔒）が実際のロック/解除に追従すること。
4. クリップボードが全画面で同期し、ループしないこと。
5. Ctrl+Alt+Pause で全キャプチャが即停止しローカル入力が回復すること。

## 既知の制限

- リモート端末の lock 状態報告は未対応（local のみ）。2D エディタは保存レイアウトの読込（編集器復元）未対応。
- セキュアデスクトップ（ロック/UAC/Ctrl+Alt+Del）・管理者アプリ（UIPI）への注入は OS 制約で不可。
- かな漢字 IME 変換（未確定 composition）取り込みは未実装。

> プレリリース。実運用前に 2〜3 台での実機検証を完了してください。
