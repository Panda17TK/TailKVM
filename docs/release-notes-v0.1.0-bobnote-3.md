# TailKVM v0.1.0 — Bob-note verification build #3

`v0.1.0-bobnote-2` の後続プレリリース。新しい **opt-in 機能**（既定 OFF・既存挙動は不変）を追加。
`claude/pdca-tailkvm-software-kvm` ブランチから生成。

## bobnote-2 からの主な変更（いずれも opt-in / 既定 OFF）

- **[機能] Raw Input マウス（PoC）**: capture の「Raw Input mouse (PoC)」チェックボックスを ON にすると、
  remote 移動量を HID 相対デルタから取得（ポインタ加速・warp フィードバックの影響を受けにくい）。
  取得失敗時は従来の cursor-warp 方式へ自動フォールバック。
- **[機能] Resolve characters (JIS/US bridge)**: 「Resolve characters」チェックボックスを ON にすると、
  - 印字キーを controller のレイアウトで `ToUnicodeEx` 解決し Unicode 送出 → **JIS/US 記号位置差を吸収**、
  - **Win / Alt+Tab / Ctrl 系ショートカット / 制御・ナビ・ファンクションキー**は物理経路で従来どおり、
  - **半角/全角・変換・無変換・かな・Kanji** は転送せず（receiver の IME を反転させない）。
- 上記は既定 OFF。OFF のままなら従来の scan/vk 物理転送（レイアウト一致環境向け）で挙動不変。

## 既定 OFF のとき

bobnote-2 と同じ挙動（マウスは cursor-warp、キーボードは全キー物理転送）。

## 品質ゲート（このビルド時点）

- `cargo fmt` / `cargo check --workspace` / `cargo clippy --workspace`（tailkvm-win32 は warning ゼロ） / `npm run build`: ✅
- `cargo test --workspace`: ✅ 0 failed（key_class / relative_delta 等のユニットテスト含む）。

## 以前のリリースノート / 検証手順

- 機能全体・既知制限: `docs/release-notes-v0.1.0-bobnote.md`、`docs/release-notes-v0.1.0-bobnote-2.md`。
- 単体マシン検証: `docs/single-machine-testing.md`。2 台検証: `TASK_LOG.md` 各タスク。
- キーボード設計と実装状況: `docs/keyboard-layout-ime-design.md` §9。

## 特に実機（2 台）で確認してほしい点

1. **Raw Input mouse** ON: remote 操作がちらつかず滑らかに追従し、停止で復帰すること。
2. **Resolve characters** ON: US↔JIS で記号（`@ [ ] :` 等）が正しく入る、Ctrl+C / Win+X / Alt+Tab が効く、
   半角/全角 で receiver の入力が壊れないこと。
3. bobnote-2 で入った failsafe 修正・単一接続（最新優先）も併せて確認。

## 既知の制限

- **かな漢字 IME 変換（未確定 composition）の取り込みは未実装**（隠しウィンドウ IME PoC が必要）。
  Resolve characters は IME-OFF の直接入力＋記号レイアウト差の吸収まで。
- CapsLock 未考慮（大文字は Shift で）。Ctrl+Alt+Del / UAC / ロック画面は OS 制約あり。

> プレリリース。実運用前に実機検証を完了してください。
