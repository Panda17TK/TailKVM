# TailKVM

**1組のマウス・キーボード・クリップボードを、[Tailscale](https://tailscale.com/) 経由で複数の Windows PC で共有。**

[![CI](https://github.com/Panda17TK/TailKVM/actions/workflows/ci.yml/badge.svg)](https://github.com/Panda17TK/TailKVM/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Panda17TK/TailKVM?include_prereleases&sort=semver)](https://github.com/Panda17TK/TailKVM/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows 11](https://img.shields.io/badge/platform-Windows%2011-0078D6)](#動作要件)

[English README is here / 英語版はこちら](README.md)

TailKVM は**ソフトウェア KVM スイッチ**です。目の前の PC のマウスとキーボードで
2台目の Windows PC を操作し、クリップボードも共有できます — 追加ハードウェアは
不要です。カーソルを画面の端の外へ動かすとリモートマシンへ越境し、戻せば手元の
PC に帰ってきます。通信はすべてプライベートな **tailnet** 上を流れるため、
2台のマシンは Tailscale で到達できれば十分です。

---

## 特長

- **シームレスなエッジ越境** — 設定した画面端へカーソルを滑らせるだけでリモート
  PC を操作開始。戻すだけで復帰。ホットキーの切り替えは不要です。
- **マルチモニタ対応** — モニタ単位のエッジ検出、Per-Monitor-V2 DPI、解像度混在、
  Windows 仮想スクリーン座標に対応。相手 PC を角に配置すれば**縦横どちらの辺
  からでも**越境できます。
- **キーボード再現性** — JIS/US レイアウト、IME・全角/半角、Win キーや Alt+Tab に
  対応。切断時には押しっぱなしキーを自動解放します。
- **日本語 IME 対応** — 文字解決 ON で半角/全角キーを押すと composition mode に
  入り、かな漢字変換は手元のローカル IME で行って**確定文字列だけ**を相手 PC へ
  送信します。候補ウィンドウは入力位置の近くに表示され、終了時には IME の状態を
  復元します。
- **クリップボード共有** — コントローラとレシーバの間で双方向に同期できます。
- **ポインタ速度調整** — 越境後の解像度差を補正するゲイン設定。
- **トレイ常駐** — システムトレイに静かに常駐。番号付きの Quick Start パネルが
  受信 → 接続 → 配置 → 操作 の順に案内します。
- **Tailscale ネイティブ転送** — TCP（既定ポート `47110`）、Tailscale の CGNAT
  レンジ（`100.64.0.0/10`）に限定。

## 動作要件

- **Windows 11**（x64）。TailKVM は Win32 の入力/フック/モニタ API を使用する
  Windows 専用アプリです。
- **[Tailscale](https://tailscale.com/)** が**両方の PC** にインストール・サインイン
  済みで、2台が同じ tailnet 上にあること。

## インストール

1. [**Releases**](https://github.com/Panda17TK/TailKVM/releases) ページから最新の
   インストーラ（`TailKVM_x.y.z_x64-setup.exe`）をダウンロードします。
2. **両方の PC**（操作する側・される側）で実行します。
3. TailKVM を起動すると、システムトレイに常駐し Quick Start パネルが開きます。

> 受信側では、tailnet からの `47110` 受信を許可するファイアウォール規則が一度
> だけ必要になる場合があります — 詳細設定の **Install firewall rule** ボタンから
> 追加できます。

## クイックスタート

Quick Start パネルは番号付きのコンソールです:

1. **RX — Receive**（操作される側の PC で）: **Start receiver** をクリックして
   コントローラからの接続を待ち受けます。
2. **01 — Connect**（操作する側の PC で）: 受信側の Tailscale IP を入力（または
   候補リストから選択）して **Connect** をクリックします。
3. **02 — Position**: モニタマップ上の **相手PC / peer** タイルをドラッグして、
   リモート画面が自分の画面に対してどこにあるかを配置します。越境する辺が
   マップに表示されます。
4. **03 — Control**: **Start KVM** をクリック。設定した辺の外へカーソルを動かすと
   リモート PC を操作でき、戻せば復帰します。動きが遅く感じる場合は
   **pointer speed** を調整してください。

フェイルセーフ: **Ctrl + Alt + Pause** で全キャプチャを即時停止できます。

### 日本語入力（composition mode）

1. 詳細設定で **Resolve characters (JIS/US bridge)** を ON にします。
2. リモート操作中に **半角/全角** キーを押すと composition mode に入ります
   （ステータスに `IME composition mode: armed` と表示）。
3. ローマ字入力 → 変換キーで候補選択 → Enter で確定すると、確定した文字列だけが
   相手 PC に入力されます。変換・無変換・かなキーは通常どおり IME 操作に使えます。
4. もう一度 半角/全角 を押すとモードを抜け、元の IME 状態とフォーカスが復元
   されます。候補ウィンドウの位置や IME ポリシーは「日本語IME入力」設定から
   変更できます。

## ソースからビルド

前提: [Rust](https://rustup.rs/)（stable）、[Node.js](https://nodejs.org/) 18+、
[Tauri の Windows 前提条件](https://tauri.app/start/prerequisites/)
（WebView2 + MSVC ビルドツール）。

```bash
git clone https://github.com/Panda17TK/TailKVM.git
cd TailKVM/apps/tailkvm-ui
npm install

# 開発（UI ホットリロード）
npm run tauri dev

# 本番ビルド（インストーラは target/release/bundle/ 配下）
npm run tauri build
```

> **重要:** デスクトップアプリは必ず **`npm run tauri build`** でビルドして
> ください。素の `cargo build --release` では開発サーバの URL
> （`localhost:1420`）が焼き込まれ、パッケージ版が UI を読み込めなくなります。

## 仕組み

```
apps/tailkvm-ui/        Tauri v2 デスクトップアプリ
  src/                  TypeScript UI（Quick Start、モニタマップ、ステータス）
  src-tauri/            Rust バックエンド: IPC コマンド、キャプチャループ、トレイ
crates/
  tailkvm-core/         共有コア型
  tailkvm-win32/        Win32 ラッパー: モニタ、カーソル、フック、座標計算
  tailkvm-net/          ワイヤプロトコル + Tailscale TCP トランスポート
```

コントローラは Raw Input のマウス差分を、ローカルモニタ群とリモート画面を
合成した**結合スクリーン空間**に積分します。カーソルが越境辺に達すると、
ローカルカーソルを固定・拘束し、入力を `WireMessage`（マウス移動/ボタン/
ホイール、キー/テキスト、クリップボード）としてレシーバへ転送します。
レシーバは `SendInput` でイベントを注入します。ハートビートがセッションを
維持し、受信側は「最新優先」の単一スロットで古いセッションを置き換えます。

設計メモ（キーボード/IME、Raw Input、マルチクライアント、OS 制約）は
[`docs/`](docs/) を参照してください。

## セキュリティ

TailKVM は実際のマウス・キーボード入力をマシン間で転送します。**自分が所有し
信頼するマシン同士を、自分の tailnet 上でのみ接続してください。** 通信は
Tailscale の CGNAT レンジに限定されています。脅威モデルと脆弱性の報告方法は
[SECURITY.md](SECURITY.md) を参照してください。

## 制限事項

- Windows 11 専用（macOS/Linux 非対応）。
- UAC 昇格画面などセキュアデスクトップには入力を注入できません。
- 安定した tailnet 上での利用を推奨。短時間の切断は自動再接続します。

## コントリビュート

Issue / PR を歓迎します — セットアップ、コードスタイル
（`cargo fmt` / `clippy` / `tsc`）、PR の流れは
[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

## ライセンス

[MIT](LICENSE) © 2026 Taiki Handa.
