# 設計: N-client ランタイム（roadmap B1）

- 日付: 2026-06-02
- ステータス: Draft（設計のみ。実装は段階タスクで別途）
- 前提実装: `screen_space`（結合座標・絶対カーソル）, `layout_graph`（名前付き隣接）,
  `run_controller_session` / `handle_receiver_stream`（1:1 セッション）, seamless 捕捉エンジン。

3 台以上で Synergy 相当のシームレス切替を行う「N-client ランタイム」の設計。
B2（`layout_graph`）の上に構築する。

---

## 1. 用語と役割

- **Server**（= 現 controller / 入力元）: 物理マウス・キーボードを持つ 1 台。論理カーソルの権威。
- **Client**（= 現 receiver / 操作先）: 入力を注入される側。0..N 台。
- **Screen**: 各ホストの仮想スクリーン。名前（`machine_name`）で識別。Server 自身も 1 screen。

現状の用語「controller/receiver」はそのまま 1:1 セッション層として再利用する。

---

## 2. 接続トポロジの決定

**決定: 現状の方向を維持し、Server が各 Client へ接続する（Client が listen）。**

- 各 Client は従来どおり `start_tcp_receiver` で listen し `handle_receiver_stream` で処理。
- Server は制御したい Client ごとに `run_controller_session` を 1 本張る（N 本同時）。
- **結果: 基本 N-client に wire プロトコル変更は不要。** 宛先の区別は「どの TCP 接続へ送るか」で行い、
  `WireMessage` に screen ID を足す必要がない。各セッションは独立した送信チャネルを持つ。

代替案（Client が Server へ接続＝Synergy 流）は ACL/発見が楽だが、現コードの方向反転が必要で利得小。不採用。

---

## 3. データモデル

### 3.1 MultiScreenSpace（`screen_space` の N 拡張・純ロジック）

現 `CombinedSpace`（2 画面）を N 画面へ一般化する新型を追加（既存 2 画面版は N=2 の特殊化として温存）。

```text
struct ScreenRect { name: String, rect: Rect }     // 各 screen の native 仮想座標
struct MultiScreenSpace { screens: HashMap<String, Rect>, graph: LayoutGraph }
struct Cursor { screen: String, x: i32, y: i32 }

impl MultiScreenSpace {
    fn apply_delta(&self, cur: Cursor, dx, dy) -> (Cursor, Option<Switch>)
    // 現 screen 内でクランプ。エッジ越え時 graph.neighbor(screen, edge) を引き、
    // 隣接 screen の対辺へ「比率マッピング」した点で進入。隣接が無ければクランプ。
}
struct Switch { from: String, to: String }
```

- 進入点の比率マッピングは現 `enter_remote/enter_local` を「任意 2 画面間」に一般化したもの。
- これは純ロジック＝ユニットテスト容易（A2「全エッジ同時」はこの apply_delta が自然に内包）。

### 3.2 ScreenLayout（永続設定・roadmap F3 / B2）

```text
struct ScreenConfig { name: String, addr: Option<String>, width: i32, height: i32, is_local: bool }
struct SavedLayout { screens: Vec<ScreenConfig>, links: Vec<(String, Edge, String)> }
```

- Tauri ストレージに保存。起動時ロード→`LayoutGraph` と `MultiScreenSpace` を構築、各 Client へ接続。
- `addr` は Tailscale IP（F1 discover で補完可）。

---

## 4. サーバ側ランタイム（Router）

新規 async タスク `run_router`（seamless エンジンの N 拡張）。AppState に `Router` 状態を持つ。

### 4.1 状態

```text
sessions: HashMap<String /*screen*/, UnboundedSender<WireMessage>>  // 各 Client へ
space: MultiScreenSpace
cursor: Cursor                 // 論理カーソル（権威）
active: String                 // 現在入力が向く screen 名（local 含む）
local_name: String
```

### 4.2 メインループ（Raw Input 駆動・低負荷）

1. failsafe（Ctrl+Alt+Pause）チェック → 全停止。
2. `active == local`:
   - 実カーソルを追従（GetCursorPos）。エッジ到達 + `SwitchGuard` 合格 → `space.apply_delta` で
     隣接 screen を解決。隣接が **remote** なら active 切替 → 当該 session へ `MouseSetPosition`、
     ローカルカーソル confine、hook 開始。隣接が無ければ何もしない。
3. `active == 某 remote`:
   - Raw Input デルタを積分 → `space.apply_delta`。
   - switch が出たら遷移:
     - 遷移先が local → confine 解放・hook 停止・実カーソル配置。
     - 遷移先が別 remote → 旧 session の hook 停止せず**転送先 sender を差し替えるだけ**
       （hook はサーバ側に 1 セットで足りる）、新 session へ `MouseSetPosition`。
   - switch 無ければ active session へ絶対 `MouseSetPosition`。
4. hook（クリック/ホイール/キーボード）転送は **active session の sender** に向ける。
   → 既存 `start_*_hook_forwarding` の sender を「現在の active sender」に動的解決する形へ一般化
   （`Arc<Mutex<Option<Sender>>>` を active で更新、フック転送ループはそれを参照）。

### 4.3 失敗時/フェイルセーフ

- Ctrl+Alt+Pause: router ループ break → confine 解放・全 hook 停止・active=local・全 session 維持 or 切断。
- ある Client の session 切断: その screen を一時的に「到達不能」マークし、active がそこなら local へ強制復帰。
  自動再接続（F2）が裏で復旧を試みる。

---

## 5. プロトコルへの追加（最小）

基本は不要だが、運用品質のため任意で:

- `Hello` の `machine_name` を screen 名として採用済み（追加不要）。
- （任意）`ScreenInfo { name, virtual_width, virtual_height, monitors }` を Hello 後に交換し、
  Server が実サイズで `MultiScreenSpace` を補正（A3 リモート実マルチモニタ対応と統合）。
- クリップボード（D1）は現状 point-to-point。N では **全 session へブロードキャスト**＋ echo guard。
  受信側 apply は現状どおり。ブロードキャストは Server がハブになる（Client 間直接は無し）。

---

## 6. 既存 1:1 との互換

- 現 seamless モード（2 画面）は **N=2 の MultiScreenSpace** として再表現できる。
  移行時は「local + 1 remote、right リンク」を自動生成すれば挙動同一。
- レガシー warp / raw-relative / mirror モードは温存（opt-in トグルのまま）。Router は新トグル
  `multi_screen` で有効化。

---

## 7. 段階実装計画（小さく検証可能に）

| Phase | 内容 | 検証 | 状態 |
| --- | --- | --- | --- |
| B1.1 | `MultiScreenSpace`（N 一般化 + `Switch`）を純ロジックで追加 | ユニットテスト（2/3 画面・全エッジ・クランプ） | ✅ 実装 (`bb10e29`) |
| B1.2 | 複数 Client への同時接続管理（`sessions: HashMap<name, Sender>`、各々 `run_controller_session` + F2 再接続） | check/build、手動 2〜3 台 | ✅ 実装 (`afcefb6`) |
| B1.3 | hook 転送 sender の「active 動的解決」化（`SenderTarget::Active`） | 既存 1:1 が不変であることを回帰 | ✅ 実装 (`50ac8ec`) |
| B1.4 | `run_router`（MultiScreenSpace + active 遷移、hook の active 切替）opt-in 配線 | 純部分テスト + 実機 | ✅ 実装 (`64550c3`、実機未検証) |
| B1.5 | クリップボード N ブロードキャスト（`broadcast_clipboard`） | 実機 | ✅ 実装 (`c91fd61`) |
| B1.6 | `SavedLayout` 永続化（F3、JSON）+ 起動時自動接続 + 設定 UI | UI/手動 | ✅ 実装 (`bf8ca01`、GUI は JSON エディタ) |
| B1.7 | `ScreenInfo` 交換でリモート実サイズ補正（A3 統合） | 実機 | ✅ 実装 (`67b959b`) |

> **B1.1〜B1.7 すべて実装済み**・全静的検証 green。N-client ランタイムは機能的に完成。
> 残: **3 台実機検証**（必須）。
>
> 後続改善も実装済み（2026-06-02）:
> - ✅ C1 dwell/dead-corner を router へ適用（`2dc3c9e`）
> - ✅ client→sibling クリップボード relay（サーバ hub、`ffd2d63`）
> - ✅ モニタ hotplug 再同期（receiver が ScreenInfo 再送）+ OS 制約ドキュメント `docs/os-limitations.md`（`c3fcfd1`）
> - ✅ 視覚的レイアウトエディタ（左→右カード、`974101e`）。本格 2D ドラッグ配置は将来。
> 残課題: 稼働中 router の MultiScreenSpace ライブ再構築、per-monitor DPI 厳密マッピング、
> ロック検知の UI 表示、本格 2D 配置エディタ。

各 Phase は既存テスト全 green を維持し、opt-in で既定挙動を壊さない。

---

## 8. 未解決事項 / リスク

1. hook はサーバに 1 セット。active 切替時の押下中キー/ボタンの引き継ぎ（遷移時に解放するか保持か）。
   → 遷移時は安全側で「旧 active へ解放を送ってから新 active へ」。stuck 防止を最優先。
2. 各 Client の DPI/解像度差・実マルチモニタ（A3）を `ScreenInfo` で正確に取り込む順序。
3. レイアウトの整合性（リンクの矛盾・島）検出と UI 警告。
4. 多数 Client 時の `tcp_snapshot`（単一スナップショット）の表現 → screen 別状態の配列化が要るか。
5. セキュリティ: N 接続それぞれに G1 の accept トグル/許可リストをどう適用するか。
6. 実機 3 台の検証導線（Bob-note + 追加 1 台）。

---

## 9. まとめ

- **基本 N-client は wire プロトコル変更不要**（接続ごとに送り分け）。最大の新規実装は
  `MultiScreenSpace`（純・テスト可）と `run_router`（active 解決 + 遷移）。
- 既存 `layout_graph` / `screen_space` / seamless エンジン / F2 再接続を素直に積み上げる。
- B1.1（MultiScreenSpace）から着手するのが最小リスク・最高レバレッジ。
