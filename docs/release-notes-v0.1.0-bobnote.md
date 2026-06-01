# TailKVM v0.1.0 — Bob-note verification build

Windows 11 + Tailscale-first Software KVM（Rust + Tauri v2、タスクトレイ常駐）の
**実機 2 台検証用プレリリース**ビルド。`claude/pdca-tailkvm-software-kvm` ブランチから生成。

## 含まれるもの

- **インストーラ**: MSI (`TailKVM_0.1.0_x64_en-US.msi`) と NSIS (`TailKVM_0.1.0_x64-setup.exe`)。
- マウス: 移動 / クリック / ダブルクリック / 右 / 中 / XButton1・2 / ホイール / ドラッグ。
- キーボード: テキスト注入（Unicode/サロゲートペア）/ 単発キー / WH_KEYBOARD_LL フックキャプチャ。
- remote mode: 画面端切替・元 PC へ戻る・`Ctrl+Alt+Pause` フェイルセーフ（二重化）。
- ディスプレイ配置エディタ、モニタ DPI / 解像度差吸収 / 仮想スクリーン座標対応。
- キーボードレイアウト識別（JIS/US/入力ロケール）+ 不一致警告。
- クリップボード共有（テキスト、echo ループ防止基盤付き）。
- 受信側 stuck key/button セーフティネット（切断時の押下解放）。
- Firewall rule 自動設定（RemoteAddress 既定 `100.64.0.0/10` = Tailscale CGNAT）。

## 品質ゲート（このビルド時点）

- `cargo fmt --all` / `cargo check --workspace` / `npm run build`: ✅
- `cargo test --workspace`: ✅ 26 passed + 1 ignored（実クリップボード FFI は `--ignored` で別途 pass 確認）。
- clippy `too_many_arguments`: 解消済み。

## 検証手順

- 単体マシン検証: `docs/single-machine-testing.md`（L1 トランスポート / L2 クリップボード FFI / L3 GUI スモーク）。
- 2 台（操作元 + Bob-note）検証: `TASK_LOG.md` 各タスクの「実機検証手順」。

## 既知の制限 / 未検証

- マウス移動キャプチャ・画面端切替・WH_*_LL ローカル抑止・Tailscale 越し疎通・Firewall rule は
  **2 台での実機検証が必要**（本リリースは未検証項目を含むプレリリース）。
- Ctrl+Alt+Del、UAC/ロック画面、管理者権限アプリへの注入は OS 制約あり（`docs/keyboard-layout-ime-design.md` 参照）。
- IME 変換状態 / 半角全角 / Win / Alt+Tab の完全対応は設計段階（同上）。

> プレリリース。実運用前に実機検証を完了してください。
