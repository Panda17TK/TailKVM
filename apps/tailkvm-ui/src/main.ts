import "./styles.css";
import { invoke } from "@tauri-apps/api/core";

type TailnetNode = {
  id: string;
  host_name: string;
  dns_name?: string | null;
  os?: string | null;
  online: boolean;
  active?: boolean | null;
  tailscale_ips: string[];
  user?: string | null;
  relay?: string | null;
  cur_addr?: string | null;
  last_seen?: string | null;
  tx_bytes?: number | null;
  rx_bytes?: number | null;
};

type TailnetStatus = {
  backend_state: string;
  self_node?: TailnetNode | null;
  peers: TailnetNode[];
  raw_peer_count: number;
};

type RectI32 = {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
};

type MonitorInfo = {
  id: string;
  name: string;
  rect_physical_px: RectI32;
  work_area_physical_px: RectI32;
  dpi_x: number;
  dpi_y: number;
  scale_factor: number;
  is_primary: boolean;
};

type MonitorTopology = {
  virtual_screen: RectI32;
  monitors: MonitorInfo[];
};

type KeyboardLayoutInfo = {
  hkl: number;
  language_id: number;
  primary_language: number;
  is_japanese_locale: boolean;
  keyboard_type: number;
  keyboard_subtype: number;
  function_keys: number;
  is_jis_keyboard: boolean;
  label: string;
};

type TcpSessionSnapshot = {
  role: string;
  listening: boolean;
  listen_addr?: string | null;
  connected: boolean;
  peer_addr?: string | null;
  peer_name?: string | null;
  heartbeat_seq: number;
  last_heartbeat_ms?: number | null;
  last_event: string;
  local_keyboard_layout?: string | null;
  peer_keyboard_layout?: string | null;
  keyboard_layout_warning?: string | null;
  ime_mode?: string;
};

const DEFAULT_PORT = 47110;
const LAYOUT_STORAGE_KEY = "tailkvm.displayLayout.v1";

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** Reject if a promise does not settle within `ms`, so a hung invoke can't
 * leave a panel spinning forever (and lets withRetry actually retry it). */
function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((_, reject) =>
      setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms),
    ),
  ]);
}

/** Retry an async operation a few times with a short delay between attempts. */
async function withRetry<T>(fn: () => Promise<T>, attempts = 6, delayMs = 350): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await fn();
    } catch (error) {
      lastError = error;
      if (attempt < attempts - 1) {
        await sleep(delayMs);
      }
    }
  }
  throw lastError;
}

let latestTailnetStatus: TailnetStatus | null = null;
let latestMonitorTopology: MonitorTopology | null = null;
// True while seamless KVM capture is armed, so the flow stops pulsing "start".
let kvmActive = false;
// Previous connection state, for one-shot "connection succeeded" effects.
let wasConnected = false;

type LayoutRect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

type SavedDisplayLayout = {
  targetPeerIp: string;
  targetPeerHost: string;
  remoteRect: LayoutRect;
  switchEdge: "left" | "right" | "top" | "bottom";
};

type LayoutDragState = {
  startClientX: number;
  startClientY: number;
  startRect: LayoutRect;
};

let layoutDragState: LayoutDragState | null = null;

const app = document.querySelector<HTMLDivElement>("#app")!;

app.innerHTML = `
  <main class="shell">
    <section class="hero">
      <div>
        <p class="eyebrow">Windows 11 + Tailscale Software KVM</p>
        <h1>TailKVM</h1>
        <p class="lead">
          複数の Windows PC でマウス・キーボード・クリップボードを Tailscale 経由で共有します。
        </p>
      </div>
      <div class="hud">
        <div class="hud-cell">
          <span class="hud-k">SELF NODE</span>
          <span class="hud-v mono" id="hud-self">—</span>
        </div>
        <div class="hud-cell">
          <span class="hud-k">LINK</span>
          <span class="hud-v" id="hud-link"><i class="hud-lamp"></i>OFFLINE</span>
        </div>
        <div class="hud-cell">
          <span class="hud-k">PEERS</span>
          <span class="hud-v mono" id="hud-peers">0</span>
        </div>
        <div class="status-pill">TRAY READY</div>
      </div>
    </section>

    <section class="card full quick-start">
      <h2>クイックスタート / Quick start</h2>
      <details class="qs-desc">
        <summary>使い方 / How to use</summary>
        <p class="qs-help">
          <b>操作する側</b>：① 相手PCの Tailscale IP を入れて接続 → ② 相手の位置をドラッグで指定 →
          ③「KVM操作を開始」。マウスを指定した<b>画面端まで動かすと相手PCを操作</b>でき、端で戻ると自分に戻ります。<br />
          <b>操作される側</b>：このPCを操作させるなら「受信を開始」を押して待ち受けます。
        </p>
      </details>

      <p class="qs-help">このPCの Tailscale IP（相手側で入力する値）: <b id="qs-self-ip">取得中...</b></p>

      <div class="qs-row" data-step="RX">
        <span class="qs-inline-label">このPCを操作される側にする：</span>
        <button id="qs-receiver">受信を開始 / Start receiver</button>
        <span id="qs-receiver-state" class="qs-state"></span>
      </div>

      <div class="qs-row" data-step="01">
        <input id="qs-host" type="text" placeholder="100.x.y.z (相手PCの Tailscale IP)" />
        <button id="qs-connect">接続 / Connect</button>
        <span id="qs-conn" class="qs-state">未接続</span>
      </div>

      <div class="qs-row qs-monitors-row" data-step="02">
        <strong>相手PC の位置 ／ このPCのモニター構成</strong>
        <div id="qs-monitors" class="qs-monitors">読込中...</div>
      </div>

      <div class="qs-kvm" data-step="03">
        <div class="qs-inline-label qs-kvm-hint">
          上のモニタ地図で<b>相手PCタイルをドラッグ</b>して位置を決め、「KVM操作を開始」。
        </div>
        <div class="qs-kvm-controls">
          <button id="qs-kvm-start">KVM操作を開始</button>
          <button id="qs-kvm-stop">停止 / Stop</button>
          <button id="qs-emergency" title="全転送停止＋カーソル解放＋被制御セッション切断（トレイの Emergency reset と同じ）">緊急リセット</button>
          <label class="qs-speed">
            操作速度
            <input id="qs-kvm-gain" type="range" min="0.5" max="4" step="0.1" value="1.8" />
            <span id="qs-kvm-gain-val">1.8×</span>
          </label>
          <span id="qs-status" class="qs-state"></span>
        </div>
      </div>

      <details class="qs-checklist-details">
        <summary>接続できない時のチェック（「connection refused」等）</summary>
        <ul>
          <li>① <b>相手PC（操作される側）でも TailKVM を起動</b>し「受信を開始」している。</li>
          <li>② 相手PCで <b>Install firewall rule</b> を一度実行（47110 の受信許可）。詳細設定にあります。</li>
          <li>③ 入れる IP は<b>相手PCの Tailscale IP</b>（このPCのIPではない）。</li>
        </ul>
      </details>

      <div class="qs-toggles">
        <button id="qs-toggle-status" class="qs-advanced-toggle" type="button">
          状態（Runtime / Tailscale / Keyboard / モニタ / Peers）を表示 ▼
        </button>
        <button id="qs-toggle-advanced" class="qs-advanced-toggle" type="button">
          詳細設定（テスト/ルータ/Raw入力/クリップボード）を表示 ▼
        </button>
      </div>
    </section>

    <section class="grid">
      <article class="card status-card">
        <h2>Runtime</h2>
        <p id="runtime-status">Not checked yet.</p>
        <button id="check-status">Check Rust backend</button>
      </article>

      <article class="card status-card">
        <h2>Tailscale</h2>
        <p id="tailscale-summary">Not loaded yet.</p>
        <button id="refresh-tailscale">Refresh peers</button>
      </article>

      <article class="card status-card">
        <h2>Keyboard Layout</h2>
        <p id="keyboard-layout-summary">Not checked yet.</p>
        <button id="refresh-keyboard-layout">Check keyboard layout</button>
      </article>

      <article class="card full advanced">
        <h2>TCP Session（詳細 / Advanced）</h2>
        <p id="tcp-summary">Not started yet.</p>

        <div class="tcp-controls">
          <label>
            Peer Tailscale IP
            <input id="tcp-host" type="text" placeholder="100.x.y.z" />
          </label>

          <label>
            Port
            <input id="tcp-port" type="number" value="47110" min="1" max="65535" />
          </label>

          <button id="start-receiver">Start receiver</button>
          <button id="connect-peer">Connect peer</button>
          <button id="disconnect-peer">Disconnect</button>
          <button id="discover-peers">Discover peers</button>
          <button id="refresh-tcp">Refresh TCP state</button>

          <label class="checkbox-label">
            <input id="accept-incoming" type="checkbox" checked />
            Accept incoming connections
          </label>

          <div id="discovered-peers" class="tcp-state empty">No discovery yet.</div>
          <div id="lock-state" class="tcp-state empty">Local input: unknown</div>

          <label>
            Screen name (multi)
            <input id="screen-name" type="text" placeholder="peer-pc" />
          </label>
          <label>
            Screen host
            <input id="screen-host" type="text" placeholder="100.x.y.z" />
          </label>
          <button id="connect-screen">Connect screen</button>
          <button id="disconnect-screen">Disconnect screen</button>
          <button id="list-screens">List screens</button>
          <div id="screen-list" class="tcp-state empty">No screens.</div>

          <label>
            Local screen name
            <input id="router-local-name" type="text" value="local" />
          </label>
          <button id="start-router">Start router (right-chain)</button>
          <button id="stop-router">Stop router</button>

          <label>
            Saved layout (JSON)
            <textarea id="layout-json" rows="6" spellcheck="false"
              placeholder='{"screens":[{"name":"local","is_local":true},{"name":"bob","host":"100.x.y.z","width":1920,"height":1080}],"links":[{"from":"local","edge":"right","to":"bob"}],"auto_connect":false}'></textarea>
          </label>
          <button id="load-layout">Load layout</button>
          <button id="save-layout">Save layout</button>

          <div class="layout-editor">
            <h4>Visual layout (local on the left, screens chained right)</h4>
            <div id="le-row" class="le-row"></div>
            <label>
              Add screen name
              <input id="le-name" type="text" placeholder="peer-pc" />
            </label>
            <label>
              host
              <input id="le-host" type="text" placeholder="100.x.y.z" />
            </label>
            <button id="le-add">Add screen</button>
            <button id="le-apply">Apply (connect all + start router)</button>
            <button id="le-reconfigure">Reconfigure live</button>
            <button id="le-save">Save visual layout</button>
          </div>

          <div class="layout-editor">
            <h4>2D placement editor (drag screens; links inferred from adjacency)</h4>
            <div id="editor-2d" class="editor-2d"></div>
            <label>
              Add screen name
              <input id="e2-name" type="text" placeholder="peer-pc" />
            </label>
            <label>
              host
              <input id="e2-host" type="text" placeholder="100.x.y.z" />
            </label>
            <button id="e2-add">Add screen</button>
            <button id="e2-reset-local">Reset to local only</button>
            <button id="e2-clear">Clear</button>
            <button id="e2-save">Save</button>
            <button id="e2-apply">Apply live</button>
          </div>

          <label>
            Firewall remote
            <input id="firewall-remote" type="text" value="100.64.0.0/10" />
          </label>

          <button id="install-firewall">Install firewall rule</button>

          <label>
            Mouse dx
            <input id="mouse-dx" type="number" value="80" min="-1000" max="1000" />
          </label>

          <label>
            Mouse dy
            <input id="mouse-dy" type="number" value="0" min="-1000" max="1000" />
          </label>

          <button id="send-mouse-test">Test mouse move</button>
          <button id="send-left-click-test">Test left click</button>
          <button id="send-right-click-test">Test right click</button>
          <button id="send-middle-click-test">Test middle click</button>
          <button id="send-x1-click-test">Test X1 click</button>
          <button id="send-x2-click-test">Test X2 click</button>
          <button id="send-left-double-click-test">Test left double click</button>
                    <label>
            Mouse gain
            <input id="mouse-gain" type="number" value="1.00" min="0.10" max="4.00" step="0.10" />
          </label>

          <label>
            Capture interval ms
            <input id="capture-interval-ms" type="number" value="8" min="8" max="100" />
          </label>

          <label>
            Max delta
            <input id="max-delta" type="number" value="80" min="10" max="500" />

          </label>

                    <label class="checkbox-label">
            <input id="remote-mode" type="checkbox" checked />
            Remote mode
          </label>

          <label class="checkbox-label">
            <input id="use-raw-input" type="checkbox" />
            Raw Input mouse (PoC)
          </label>

          <label class="checkbox-label">
            <input id="seamless-mode" type="checkbox" />
            Seamless absolute mode (PoC)
          </label>

          <label>
            Edge dwell ms (0=instant)
            <input id="edge-dwell-ms" type="number" value="0" min="0" max="2000" />
          </label>

          <label>
            Dead corner px (0=off)
            <input id="dead-corner-px" type="number" value="0" min="0" max="1000" />
          </label>

          <label>
            Switch edge
            <select id="switch-edge">
              <option value="right" selected>right</option>
              <option value="left">left</option>
              <option value="top">top</option>
              <option value="bottom">bottom</option>
            </select>
          </label>

          <label>
            Edge margin px
            <input id="edge-margin" type="number" value="3" min="1" max="64" />
          </label>

          <button id="start-mouse-capture">Capture mouse</button>
          <button id="stop-mouse-capture">Stop capture</button>

          <label>
            Keyboard text
            <input id="keyboard-text" type="text" value="hello tailkvm" maxlength="200" />
          </label>

          <button id="send-keyboard-text">Send keyboard text</button>
          <button id="send-key-enter">Test Enter</button>
          <button id="send-key-backspace">Test Backspace</button>
          <button id="send-key-tab">Test Tab</button>
          <button id="send-key-escape">Test Escape</button>

          <button id="start-keyboard-hook-capture">Capture keyboard</button>
          <button id="stop-keyboard-hook-capture">Stop keyboard capture</button>

          <label class="checkbox-label">
            <input id="resolve-characters" type="checkbox" />
            Resolve characters (JIS/US bridge)
          </label>

          <button id="send-clipboard-text">Send clipboard to peer</button>
          <button id="send-clipboard-image">Send clipboard image to peer</button>

          <label class="checkbox-label">
            <input id="clipboard-sync" type="checkbox" />
            Auto clipboard sync (bidirectional)
          </label>

          <button id="start-raw-mouse-diagnostic">Raw Input diagnostic (PoC)</button>
          <button id="stop-raw-mouse-diagnostic">Stop Raw Input diagnostic</button>
        </div>

        <div id="tcp-state" class="tcp-state empty">Not loaded yet.</div>
      </article>

      <article class="card full advanced">
        <h2>日本語IME入力（詳細 / Advanced）</h2>
        <p id="ime-status">IME composition mode: off</p>
        <p>
          文字解決ONの状態で半角/全角キーを押すと composition mode に入ります。
          変換はローカルIMEで行い、確定文字のみ相手PCへ送信します。
        </p>

        <div class="layout-controls">
          <label>
            プリセット
            <select id="ime-preset">
              <option value="" selected>（選択して一括適用）</option>
              <option value="standard_japanese">標準（日本語優先）</option>
              <option value="preserve_current">現状維持</option>
              <option value="last_session">前回の状態</option>
            </select>
          </label>

          <label>
            候補ウィンドウ位置
            <select id="ime-candidate-position">
              <option value="remote_projected">リモートカーソル位置を投影（推奨）</option>
              <option value="lock_near">ロック位置の近傍</option>
              <option value="monitor_center">現在モニタ中央</option>
              <option value="fixed">固定座標</option>
              <option value="legacy_top_left">従来互換（左上）</option>
            </select>
          </label>

          <label>
            IME open policy
            <select id="ime-open-policy">
              <option value="force_japanese">force_japanese（推奨）</option>
              <option value="preserve_current">preserve_current</option>
              <option value="restore_last_tailkvm">restore_last_tailkvm</option>
              <option value="manual">manual</option>
            </select>
          </label>

          <label>
            Conversion mode policy
            <select id="ime-conversion-policy">
              <option value="native_default">native_default（推奨）</option>
              <option value="native_fullshape">native_fullshape（互換）</option>
              <option value="preserve">preserve</option>
              <option value="last_used">last_used</option>
            </select>
          </label>

          <label>
            フォーカス取得失敗時
            <select id="ime-focus-policy">
              <option value="retry">retry（推奨）</option>
              <option value="warn_continue">warn_continue</option>
              <option value="abort">abort</option>
            </select>
          </label>

          <label>
            固定座標 X（fixed 用）
            <input id="ime-fixed-x" type="number" value="0" step="1" />
          </label>

          <label>
            固定座標 Y（fixed 用）
            <input id="ime-fixed-y" type="number" value="0" step="1" />
          </label>

          <label>
            capture window サイズ(px)
            <select id="ime-window-size">
              <option value="1">1（既定）</option>
              <option value="2">2</option>
              <option value="8">8</option>
            </select>
          </label>

          <label>
            lock_near オフセット(px)
            <input id="ime-lock-offset" type="number" value="24" min="0" max="256" step="1" />
          </label>
        </div>
      </article>

      <article class="card full advanced">
        <h2>Display Layout Editor（詳細 / Advanced）</h2>
        <p id="layout-summary">
          Arrange the remote display like Windows display settings. This layout will be used for edge mapping.
        </p>

        <div class="layout-controls">
          <label>
            Target peer
            <select id="layout-peer">
              <option value="">Select peer...</option>
            </select>
          </label>

          <label>
            Remote width
            <input id="layout-remote-width" type="number" value="1920" min="640" max="10000" />
          </label>

          <label>
            Remote height
            <input id="layout-remote-height" type="number" value="1080" min="480" max="10000" />
          </label>

          <label>
            Canvas scale
            <input id="layout-scale" type="number" value="0.12" min="0.03" max="0.40" step="0.01" />
          </label>

          <button id="reset-layout">Reset layout</button>
          <button id="apply-layout">Use layout</button>
        </div>

        <div id="layout-canvas" class="layout-canvas empty">
          Load monitors and Tailscale peers first.
        </div>
      </article>

      <article class="card full status-card">
        <h2>Monitor Topology</h2>
        <p id="monitor-summary">Not loaded yet.</p>
        <button id="refresh-monitors">Refresh monitors</button>
        <div id="monitor-list" class="monitor-list empty">Not loaded yet.</div>
      </article>

      <article class="card full status-card">
        <h2>This machine</h2>
        <div id="self-node" class="empty">Not loaded yet.</div>
      </article>

      <article class="card full status-card">
        <h2>Peers</h2>
        <div id="peer-list" class="peer-list empty">Not loaded yet.</div>
      </article>
    </section>
  </main>
`;

document
  .querySelector<HTMLButtonElement>("#check-status")
  ?.addEventListener("click", async () => {
    const status = await invoke<string>("get_app_status");
    document.querySelector<HTMLParagraphElement>("#runtime-status")!.textContent = status;
  });

document
  .querySelector<HTMLButtonElement>("#refresh-tailscale")
  ?.addEventListener("click", async () => refreshTailscaleStatus());

document
  .querySelector<HTMLButtonElement>("#refresh-monitors")
  ?.addEventListener("click", async () => refreshMonitorTopology());

document
  .querySelector<HTMLButtonElement>("#refresh-keyboard-layout")
  ?.addEventListener("click", async () => refreshKeyboardLayout());

document
  .querySelector<HTMLButtonElement>("#refresh-tcp")
  ?.addEventListener("click", async () => refreshTcpSession());

document
  .querySelector<HTMLButtonElement>("#install-firewall")
  ?.addEventListener("click", async () => {
    const port = getPortValue();
    const remoteAddress = document
      .querySelector<HTMLInputElement>("#firewall-remote")!
      .value
      .trim();

    try {
      const message = await invoke<string>("install_firewall_rule", {
        port,
        remoteAddress,
      });

      renderTcpInfo(`${message}\n\nUAC prompt should appear. Approve it to install the rule.`);
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#send-mouse-test")
  ?.addEventListener("click", async () => {
    const dx = getNumberInput("#mouse-dx", 80);
    const dy = getNumberInput("#mouse-dy", 0);

    await invoke<TcpSessionSnapshot>("send_test_mouse_move", { dx, dy });
    await refreshTcpSession();
  });

document
  .querySelector<HTMLButtonElement>("#start-receiver")
  ?.addEventListener("click", async () => {
    const port = getPortValue();
    await invoke<TcpSessionSnapshot>("start_tcp_receiver", { port });
    await refreshTcpSession();
  });

document
  .querySelector<HTMLButtonElement>("#connect-peer")
  ?.addEventListener("click", async () => {
    const host = document.querySelector<HTMLInputElement>("#tcp-host")!.value.trim();
    const port = getPortValue();

    if (!host) {
      renderTcpError("Peer Tailscale IP is empty.");
      return;
    }

    await invoke<TcpSessionSnapshot>("connect_tcp_peer", { host, port });
    await refreshTcpSession();
  });

document
  .querySelector<HTMLButtonElement>("#disconnect-peer")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("disconnect_tcp_peer");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLInputElement>("#accept-incoming")
  ?.addEventListener("change", async (event) => {
    const enabled = (event.target as HTMLInputElement).checked;
    try {
      await invoke<TcpSessionSnapshot>("set_accept_incoming", { enabled });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

async function refreshScreenList() {
  const box = document.querySelector<HTMLDivElement>("#screen-list")!;
  try {
    const screens = await invoke<{ name: string; connected: boolean; state: string }[]>(
      "list_screens",
    );
    box.innerHTML = screens.length
      ? screens
          .map((s) => {
            const icon = s.state === "active" ? "🟢" : "🟡";
            return `<div>${icon} ${escapeHtml(s.name)} — ${escapeHtml(s.state)}</div>`;
          })
          .join("")
      : "No screens.";
  } catch (error) {
    box.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
  }
}

async function refreshLockState() {
  const box = document.querySelector<HTMLDivElement>("#lock-state");
  if (!box) return;
  try {
    const lock = await invoke<{ locked: boolean }>("get_lock_state");
    box.textContent = lock.locked
      ? "🔒 Local input: locked / secure desktop — sharing suspended here"
      : "🟢 Local input: active";
  } catch (error) {
    box.textContent = `Local input: error (${String(error)})`;
  }
}

document
  .querySelector<HTMLButtonElement>("#connect-screen")
  ?.addEventListener("click", async () => {
    const name = document.querySelector<HTMLInputElement>("#screen-name")!.value.trim();
    const host = document.querySelector<HTMLInputElement>("#screen-host")!.value.trim();
    const port = getPortValue();
    if (!name || !host) {
      renderTcpError("Screen name and host are required.");
      return;
    }
    try {
      await invoke<TcpSessionSnapshot>("connect_screen", { name, host, port });
      await refreshScreenList();
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#disconnect-screen")
  ?.addEventListener("click", async () => {
    const name = document.querySelector<HTMLInputElement>("#screen-name")!.value.trim();
    if (!name) {
      renderTcpError("Screen name is required.");
      return;
    }
    try {
      await invoke<TcpSessionSnapshot>("disconnect_screen", { name });
      await refreshScreenList();
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#list-screens")
  ?.addEventListener("click", async () => {
    await refreshScreenList();
  });

document
  .querySelector<HTMLButtonElement>("#start-router")
  ?.addEventListener("click", async () => {
    try {
      const localName =
        document.querySelector<HTMLInputElement>("#router-local-name")!.value.trim() || "local";
      const screens = await invoke<{ name: string; connected: boolean }[]>("list_screens");
      const remoteSize = getSelectedRemoteSize();

      // Build a simple left-to-right chain: local -> screen1 -> screen2 -> ...
      const configScreens = [
        { name: localName, width: 0, height: 0, is_local: true },
        ...screens.map((s) => ({
          name: s.name,
          width: remoteSize.width,
          height: remoteSize.height,
          is_local: false,
        })),
      ];
      const chain = [localName, ...screens.map((s) => s.name)];
      const links = chain.slice(0, -1).map((from, i) => ({
        from,
        edge: "right",
        to: chain[i + 1],
      }));

      if (links.length === 0) {
        renderTcpError("Connect at least one screen before starting the router.");
        return;
      }

      const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
      const deadCornerPx = getNumberInput("#dead-corner-px", 0);
      await invoke<TcpSessionSnapshot>("start_multi_screen_router", {
        config: { screens: configScreens, links },
        edgeDwellMs,
        deadCornerPx,
      });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-router")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_multi_screen_router");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#load-layout")
  ?.addEventListener("click", async () => {
    try {
      const layout = await invoke<unknown>("load_layout");
      document.querySelector<HTMLTextAreaElement>("#layout-json")!.value = JSON.stringify(
        layout,
        null,
        2,
      );
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#save-layout")
  ?.addEventListener("click", async () => {
    const raw = document.querySelector<HTMLTextAreaElement>("#layout-json")!.value.trim();
    let layout: unknown;
    try {
      layout = JSON.parse(raw);
    } catch {
      renderTcpError("Layout JSON is invalid.");
      return;
    }
    try {
      await invoke<TcpSessionSnapshot>("save_layout", { layout });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

// --- Visual layout editor (left -> right chain) ---
type VisualScreen = { name: string; host: string };
let visualScreens: VisualScreen[] = [];

function localScreenName(): string {
  return (
    document.querySelector<HTMLInputElement>("#router-local-name")?.value.trim() || "local"
  );
}

function renderVisualLayout() {
  const row = document.querySelector<HTMLDivElement>("#le-row");
  if (!row) return;
  const localCard = `<div class="le-card le-local">🖥 ${escapeHtml(localScreenName())} (local)</div>`;
  const cards = visualScreens
    .map(
      (s, i) =>
        `<div class="le-card">` +
        `<div class="le-name">${escapeHtml(s.name)}</div>` +
        `<div class="le-host">${escapeHtml(s.host)}</div>` +
        `<div class="le-actions">` +
        `<button data-le-left="${i}" ${i === 0 ? "disabled" : ""}>←</button>` +
        `<button data-le-right="${i}" ${i === visualScreens.length - 1 ? "disabled" : ""}>→</button>` +
        `<button data-le-del="${i}">✕</button>` +
        `</div></div>`,
    )
    .join("");
  row.innerHTML = localCard + cards;
}

function buildVisualLayout() {
  const localName = localScreenName();
  const screens = [
    { name: localName, host: "", width: 0, height: 0, is_local: true },
    ...visualScreens.map((s) => ({
      name: s.name,
      host: s.host,
      width: 1920,
      height: 1080,
      is_local: false,
    })),
  ];
  const chain = [localName, ...visualScreens.map((s) => s.name)];
  const links = chain.slice(0, -1).map((from, i) => ({ from, edge: "right", to: chain[i + 1] }));
  return { screens, links, auto_connect: false };
}

document.querySelector<HTMLButtonElement>("#le-add")?.addEventListener("click", () => {
  const name = document.querySelector<HTMLInputElement>("#le-name")!.value.trim();
  const host = document.querySelector<HTMLInputElement>("#le-host")!.value.trim();
  if (!name || !host) {
    renderTcpError("Screen name and host are required.");
    return;
  }
  visualScreens.push({ name, host });
  document.querySelector<HTMLInputElement>("#le-name")!.value = "";
  document.querySelector<HTMLInputElement>("#le-host")!.value = "";
  renderVisualLayout();
});

document.querySelector<HTMLDivElement>("#le-row")?.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const del = target.getAttribute("data-le-del");
  const left = target.getAttribute("data-le-left");
  const right = target.getAttribute("data-le-right");
  if (del !== null) {
    visualScreens.splice(Number(del), 1);
  } else if (left !== null) {
    const i = Number(left);
    if (i > 0) [visualScreens[i - 1], visualScreens[i]] = [visualScreens[i], visualScreens[i - 1]];
  } else if (right !== null) {
    const i = Number(right);
    if (i < visualScreens.length - 1)
      [visualScreens[i + 1], visualScreens[i]] = [visualScreens[i], visualScreens[i + 1]];
  } else {
    return;
  }
  renderVisualLayout();
});

document.querySelector<HTMLButtonElement>("#le-save")?.addEventListener("click", async () => {
  try {
    await invoke<TcpSessionSnapshot>("save_layout", { layout: buildVisualLayout() });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
});

document.querySelector<HTMLButtonElement>("#le-apply")?.addEventListener("click", async () => {
  if (visualScreens.length === 0) {
    renderTcpError("Add at least one screen.");
    return;
  }
  const layout = buildVisualLayout();
  const port = getPortValue();
  try {
    for (const screen of visualScreens) {
      await invoke<TcpSessionSnapshot>("connect_screen", {
        name: screen.name,
        host: screen.host,
        port,
      });
    }
    const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
    const deadCornerPx = getNumberInput("#dead-corner-px", 0);
    await invoke<TcpSessionSnapshot>("start_multi_screen_router", {
      config: { screens: layout.screens, links: layout.links },
      edgeDwellMs,
      deadCornerPx,
    });
    await refreshScreenList();
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
});

// Live reconfigure: rebuild the running router's screen space without restart.
document
  .querySelector<HTMLButtonElement>("#le-reconfigure")
  ?.addEventListener("click", async () => {
    const layout = buildVisualLayout();
    try {
      await invoke<TcpSessionSnapshot>("reconfigure_router", {
        config: { screens: layout.screens, links: layout.links },
      });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

renderVisualLayout();

// --- 2D drag placement editor (issue 4) ---
type Editor2DScreen = { name: string; host: string; x: number; y: number; isLocal: boolean };
const E2_BOX_W = 120;
const E2_BOX_H = 70;
const E2_SNAP = 20;
const E2_BAND = 50; // vertical/horizontal overlap tolerance for adjacency
let editor2d: Editor2DScreen[] = [];

function resetEditor2dToLocal() {
  editor2d = [{ name: localScreenName(), host: "", x: 40, y: 40, isLocal: true }];
  renderEditor2d();
}

function renderEditor2d() {
  const canvas = document.querySelector<HTMLDivElement>("#editor-2d");
  if (!canvas) return;
  canvas.innerHTML = editor2d
    .map(
      (s, i) =>
        `<div class="e2-box${s.isLocal ? " e2-local" : ""}" data-e2="${i}" ` +
        `style="left:${s.x}px;top:${s.y}px;width:${E2_BOX_W}px;height:${E2_BOX_H}px;">` +
        `<div class="e2-name">${escapeHtml(s.name)}${s.isLocal ? " (local)" : ""}</div>` +
        `<div class="e2-host">${escapeHtml(s.host)}</div>` +
        (s.isLocal ? "" : `<button class="e2-del" data-e2-del="${i}">✕</button>`) +
        `</div>`,
    )
    .join("");
}

function inferEditor2dLinks(): { from: string; edge: string; to: string }[] {
  const links: { from: string; edge: string; to: string }[] = [];
  const center = (s: Editor2DScreen) => ({ cx: s.x + E2_BOX_W / 2, cy: s.y + E2_BOX_H / 2 });
  for (const a of editor2d) {
    const ca = center(a);
    let right: Editor2DScreen | null = null;
    let rdx = Infinity;
    let down: Editor2DScreen | null = null;
    let ddy = Infinity;
    for (const b of editor2d) {
      if (b === a) continue;
      const cb = center(b);
      const dx = cb.cx - ca.cx;
      const dy = cb.cy - ca.cy;
      if (dx > 0 && Math.abs(dy) < E2_BAND && dx < rdx) {
        right = b;
        rdx = dx;
      }
      if (dy > 0 && Math.abs(dx) < E2_BAND && dy < ddy) {
        down = b;
        ddy = dy;
      }
    }
    if (right) links.push({ from: a.name, edge: "right", to: right.name });
    if (down) links.push({ from: a.name, edge: "bottom", to: down.name });
  }
  return links;
}

function buildEditor2dLayout() {
  const screens = editor2d.map((s) => ({
    name: s.name,
    host: s.host,
    width: 1920,
    height: 1080,
    is_local: s.isLocal,
  }));
  return { screens, links: inferEditor2dLinks(), auto_connect: false };
}

(() => {
  const canvas = document.querySelector<HTMLDivElement>("#editor-2d");
  if (!canvas) return;
  let dragIndex: number | null = null;
  let offsetX = 0;
  let offsetY = 0;

  canvas.addEventListener("pointerdown", (event) => {
    const target = (event.target as HTMLElement).closest<HTMLElement>(".e2-box");
    if (!target) return;
    if ((event.target as HTMLElement).hasAttribute("data-e2-del")) return;
    const idx = Number(target.getAttribute("data-e2"));
    const rect = canvas.getBoundingClientRect();
    dragIndex = idx;
    offsetX = event.clientX - rect.left - editor2d[idx].x;
    offsetY = event.clientY - rect.top - editor2d[idx].y;
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener("pointermove", (event) => {
    if (dragIndex === null) return;
    const rect = canvas.getBoundingClientRect();
    let x = event.clientX - rect.left - offsetX;
    let y = event.clientY - rect.top - offsetY;
    x = Math.max(0, Math.round(x / E2_SNAP) * E2_SNAP);
    y = Math.max(0, Math.round(y / E2_SNAP) * E2_SNAP);
    editor2d[dragIndex].x = x;
    editor2d[dragIndex].y = y;
    renderEditor2d();
  });
  const end = (event: PointerEvent) => {
    if (dragIndex !== null) {
      dragIndex = null;
      try {
        canvas.releasePointerCapture(event.pointerId);
      } catch {
        /* ignore */
      }
    }
  };
  canvas.addEventListener("pointerup", end);
  canvas.addEventListener("pointercancel", end);

  canvas.addEventListener("click", (event) => {
    const del = (event.target as HTMLElement).getAttribute("data-e2-del");
    if (del !== null) {
      editor2d.splice(Number(del), 1);
      renderEditor2d();
    }
  });
})();

document.querySelector<HTMLButtonElement>("#e2-add")?.addEventListener("click", () => {
  const name = document.querySelector<HTMLInputElement>("#e2-name")!.value.trim();
  const host = document.querySelector<HTMLInputElement>("#e2-host")!.value.trim();
  if (!name || !host) {
    renderTcpError("Screen name and host are required.");
    return;
  }
  const maxX = editor2d.reduce((m, s) => Math.max(m, s.x), 0);
  editor2d.push({ name, host, x: maxX + E2_BOX_W + E2_SNAP, y: 40, isLocal: false });
  document.querySelector<HTMLInputElement>("#e2-name")!.value = "";
  document.querySelector<HTMLInputElement>("#e2-host")!.value = "";
  renderEditor2d();
});

document
  .querySelector<HTMLButtonElement>("#e2-reset-local")
  ?.addEventListener("click", resetEditor2dToLocal);
document.querySelector<HTMLButtonElement>("#e2-clear")?.addEventListener("click", () => {
  editor2d = [];
  renderEditor2d();
});

document.querySelector<HTMLButtonElement>("#e2-save")?.addEventListener("click", async () => {
  try {
    await invoke<TcpSessionSnapshot>("save_layout", { layout: buildEditor2dLayout() });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
});

document.querySelector<HTMLButtonElement>("#e2-apply")?.addEventListener("click", async () => {
  const remotes = editor2d.filter((s) => !s.isLocal);
  if (remotes.length === 0) {
    renderTcpError("Add at least one remote screen.");
    return;
  }
  const layout = buildEditor2dLayout();
  const port = getPortValue();
  try {
    for (const screen of remotes) {
      await invoke<TcpSessionSnapshot>("connect_screen", {
        name: screen.name,
        host: screen.host,
        port,
      });
    }
    const config = { screens: layout.screens, links: layout.links };
    try {
      await invoke<TcpSessionSnapshot>("reconfigure_router", { config });
    } catch {
      const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
      const deadCornerPx = getNumberInput("#dead-corner-px", 0);
      await invoke<TcpSessionSnapshot>("start_multi_screen_router", {
        config,
        edgeDwellMs,
        deadCornerPx,
      });
    }
    await refreshScreenList();
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
});

resetEditor2dToLocal();

document
  .querySelector<HTMLButtonElement>("#discover-peers")
  ?.addEventListener("click", async () => {
    const box = document.querySelector<HTMLDivElement>("#discovered-peers")!;
    box.textContent = "Discovering...";
    try {
      const port = getPortValue();
      const peers = await invoke<
        { host_name: string; ip: string; reachable: boolean }[]
      >("discover_tailkvm_peers", { port });
      if (peers.length === 0) {
        box.textContent = "No online peers found.";
        return;
      }
      box.innerHTML = peers
        .map(
          (p) =>
            `<div>${p.reachable ? "✅" : "—"} ${escapeHtml(p.host_name)} (${escapeHtml(p.ip)})${p.reachable ? " — TailKVM port open" : ""}</div>`,
        )
        .join("");
    } catch (error) {
      box.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
    }
  });


document
  .querySelector<HTMLButtonElement>("#send-left-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseClick("left");
  });

document
  .querySelector<HTMLButtonElement>("#send-right-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseClick("right");
  });

document
  .querySelector<HTMLButtonElement>("#send-middle-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseClick("middle");
  });

document
  .querySelector<HTMLButtonElement>("#send-x1-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseClick("x1");
  });

document
  .querySelector<HTMLButtonElement>("#send-x2-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseClick("x2");
  });

document
  .querySelector<HTMLButtonElement>("#send-left-double-click-test")
  ?.addEventListener("click", async () => {
    await sendTestMouseDoubleClick("left");
  });

// NOTE: these two buttons (#start/stop-mouse-hook-capture) are not present in
// the current DOM. Use optional chaining instead of `!` so a missing element
// becomes a no-op rather than a TypeError that aborts the rest of this module's
// top-level evaluation (which previously killed all initial data loading).
document
  .querySelector<HTMLButtonElement>("#start-mouse-hook-capture")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("start_mouse_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-mouse-hook-capture")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_mouse_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#send-keyboard-text")
  ?.addEventListener("click", async () => {
    const text = document.querySelector<HTMLInputElement>("#keyboard-text")!.value;
    await sendTestKeyboardText(text);
  });

document
  .querySelector<HTMLButtonElement>("#send-key-enter")
  ?.addEventListener("click", async () => {
    await sendTestKeyTap("enter");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-backspace")
  ?.addEventListener("click", async () => {
    await sendTestKeyTap("backspace");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-tab")
  ?.addEventListener("click", async () => {
    await sendTestKeyTap("tab");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-escape")
  ?.addEventListener("click", async () => {
    await sendTestKeyTap("escape");
  });

document
  .querySelector<HTMLInputElement>("#clipboard-sync")
  ?.addEventListener("change", async (event) => {
    const enabled = (event.target as HTMLInputElement).checked;
    try {
      await invoke<TcpSessionSnapshot>("set_clipboard_sync", { enabled });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLInputElement>("#resolve-characters")
  ?.addEventListener("change", async (event) => {
    const enabled = (event.target as HTMLInputElement).checked;
    try {
      await invoke<TcpSessionSnapshot>("set_resolve_characters", { enabled });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

// --- Japanese IME settings (IME-UI-002 / IME-CONF-001..003) ---

type ImeSettings = {
  version: number;
  candidatePositionMode: string;
  imeOpenPolicy: string;
  conversionModePolicy: string;
  focusFailurePolicy: string;
  fixedX: number;
  fixedY: number;
  captureWindowSize: number;
  lockNearOffset: number;
};

const IME_SETTINGS_KEY = "tailkvm.imeSettings.v1";

const DEFAULT_IME_SETTINGS: ImeSettings = {
  version: 1,
  candidatePositionMode: "remote_projected",
  imeOpenPolicy: "force_japanese",
  conversionModePolicy: "native_default",
  focusFailurePolicy: "retry",
  fixedX: 0,
  fixedY: 0,
  captureWindowSize: 1,
  lockNearOffset: 24,
};

// IME state presets (P2): one-click policy combinations. Fields not listed
// keep their current values.
const IME_PRESETS: Record<string, Partial<ImeSettings>> = {
  standard_japanese: {
    candidatePositionMode: "remote_projected",
    imeOpenPolicy: "force_japanese",
    conversionModePolicy: "native_default",
    focusFailurePolicy: "retry",
  },
  preserve_current: {
    imeOpenPolicy: "preserve_current",
    conversionModePolicy: "preserve",
    focusFailurePolicy: "warn_continue",
  },
  last_session: {
    imeOpenPolicy: "restore_last_tailkvm",
    conversionModePolicy: "last_used",
    focusFailurePolicy: "retry",
  },
};

function loadImeSettings(): ImeSettings {
  try {
    const raw = localStorage.getItem(IME_SETTINGS_KEY);
    if (!raw) return { ...DEFAULT_IME_SETTINGS };
    const parsed = JSON.parse(raw) as Partial<ImeSettings>;
    return { ...DEFAULT_IME_SETTINGS, ...parsed, version: 1 };
  } catch {
    return { ...DEFAULT_IME_SETTINGS };
  }
}

function readImeSettingsFromUi(): ImeSettings {
  const select = (id: string, fallback: string): string =>
    document.querySelector<HTMLSelectElement>(id)?.value ?? fallback;
  const number = (id: string): number =>
    Number(document.querySelector<HTMLInputElement>(id)?.value) || 0;
  return {
    version: 1,
    candidatePositionMode: select(
      "#ime-candidate-position",
      DEFAULT_IME_SETTINGS.candidatePositionMode,
    ),
    imeOpenPolicy: select("#ime-open-policy", DEFAULT_IME_SETTINGS.imeOpenPolicy),
    conversionModePolicy: select(
      "#ime-conversion-policy",
      DEFAULT_IME_SETTINGS.conversionModePolicy,
    ),
    focusFailurePolicy: select("#ime-focus-policy", DEFAULT_IME_SETTINGS.focusFailurePolicy),
    fixedX: number("#ime-fixed-x"),
    fixedY: number("#ime-fixed-y"),
    captureWindowSize:
      number("#ime-window-size") || DEFAULT_IME_SETTINGS.captureWindowSize,
    lockNearOffset: number("#ime-lock-offset"),
  };
}

function applyImeSettingsToUi(settings: ImeSettings): void {
  const set = (id: string, value: string): void => {
    const element = document.querySelector<HTMLSelectElement | HTMLInputElement>(id);
    if (element) element.value = value;
  };
  set("#ime-candidate-position", settings.candidatePositionMode);
  set("#ime-open-policy", settings.imeOpenPolicy);
  set("#ime-conversion-policy", settings.conversionModePolicy);
  set("#ime-focus-policy", settings.focusFailurePolicy);
  set("#ime-fixed-x", String(settings.fixedX));
  set("#ime-fixed-y", String(settings.fixedY));
  set("#ime-window-size", String(settings.captureWindowSize));
  set("#ime-lock-offset", String(settings.lockNearOffset));
}

async function pushImeSettings(settings: ImeSettings): Promise<void> {
  localStorage.setItem(IME_SETTINGS_KEY, JSON.stringify(settings));
  try {
    await invoke<TcpSessionSnapshot>("set_ime_settings", {
      settings: {
        candidatePositionMode: settings.candidatePositionMode,
        imeOpenPolicy: settings.imeOpenPolicy,
        conversionModePolicy: settings.conversionModePolicy,
        focusFailurePolicy: settings.focusFailurePolicy,
        fixedX: settings.fixedX,
        fixedY: settings.fixedY,
        captureWindowSize: settings.captureWindowSize,
        lockNearOffset: settings.lockNearOffset,
      },
    });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

function initImeSettings(): void {
  applyImeSettingsToUi(loadImeSettings());
  // Push the persisted settings to the backend on startup so composition
  // mode uses them even before the user touches the controls.
  void pushImeSettings(readImeSettingsFromUi());
  for (const id of [
    "#ime-candidate-position",
    "#ime-open-policy",
    "#ime-conversion-policy",
    "#ime-focus-policy",
    "#ime-fixed-x",
    "#ime-fixed-y",
    "#ime-window-size",
    "#ime-lock-offset",
  ]) {
    document.querySelector<HTMLElement>(id)?.addEventListener("change", () => {
      void pushImeSettings(readImeSettingsFromUi());
    });
  }
  // Preset selector: applies a policy combination on top of the current
  // values, then resets itself so it reads as an action, not a state.
  document.querySelector<HTMLSelectElement>("#ime-preset")?.addEventListener("change", (event) => {
    const select = event.target as HTMLSelectElement;
    const preset = IME_PRESETS[select.value];
    if (preset) {
      const merged = { ...readImeSettingsFromUi(), ...preset };
      applyImeSettingsToUi(merged);
      void pushImeSettings(merged);
    }
    select.value = "";
  });
}

initImeSettings();

document
  .querySelector<HTMLButtonElement>("#send-clipboard-text")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("send_clipboard_text");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#send-clipboard-image")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("send_clipboard_image");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#start-raw-mouse-diagnostic")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("start_raw_mouse_diagnostic");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-raw-mouse-diagnostic")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_raw_mouse_diagnostic");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#start-keyboard-hook-capture")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("start_keyboard_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-keyboard-hook-capture")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_keyboard_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#start-mouse-capture")
  ?.addEventListener("click", async () => {
    try {
      const gain = getFloatInput("#mouse-gain", 1.0);
      const intervalMs = getNumberInput("#capture-interval-ms", 8);
      const maxDelta = getNumberInput("#max-delta", 80);
      const remoteMode = document.querySelector<HTMLInputElement>("#remote-mode")?.checked ?? true;
      const switchEdge = document.querySelector<HTMLSelectElement>("#switch-edge")?.value ?? "right";
      const edgeMargin = getNumberInput("#edge-margin", 3);
      const remoteSize = getSelectedRemoteSize();
      const useRawInput =
        document.querySelector<HTMLInputElement>("#use-raw-input")?.checked ?? false;
      const seamless =
        document.querySelector<HTMLInputElement>("#seamless-mode")?.checked ?? false;
      const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
      const deadCornerPx = getNumberInput("#dead-corner-px", 0);

      await invoke<TcpSessionSnapshot>("start_mouse_capture", {
        gain,
        intervalMs,
        maxDelta,
        remoteMode,
        switchEdge,
        edgeMargin,
        remoteWidth: remoteSize.width,
        remoteHeight: remoteSize.height,
        useRawInput,
        seamless,
        edgeDwellMs,
        deadCornerPx,
      });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-mouse-capture")
  ?.addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_mouse_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#apply-layout")
  ?.addEventListener("click", () => {
    applyDisplayLayoutToControls();
  });

document
  .querySelector<HTMLButtonElement>("#reset-layout")
  ?.addEventListener("click", () => {
    localStorage.removeItem(LAYOUT_STORAGE_KEY);
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLSelectElement>("#layout-peer")
  ?.addEventListener("change", () => {
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-remote-width")
  ?.addEventListener("change", () => {
    updateSavedRemoteSizeFromInputs();
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-remote-height")
  ?.addEventListener("change", () => {
    updateSavedRemoteSizeFromInputs();
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-scale")
  ?.addEventListener("change", () => {
    renderDisplayLayoutEditor();
  });

document.addEventListener("pointerdown", (event) => {
  const target = event.target;

  if (!(target instanceof HTMLElement)) {
    return;
  }

  const remote = target.closest<HTMLElement>(".layout-remote");

  if (!remote) {
    return;
  }

  const layout = getCurrentDisplayLayout();

  if (!layout) {
    return;
  }

  layoutDragState = {
    startClientX: event.clientX,
    startClientY: event.clientY,
    startRect: { ...layout.remoteRect },
  };

  event.preventDefault();
});

document.addEventListener("pointermove", (event) => {
  if (!layoutDragState) {
    return;
  }

  const scale = getLayoutScale();
  const dx = (event.clientX - layoutDragState.startClientX) / scale;
  const dy = (event.clientY - layoutDragState.startClientY) / scale;

  const layout = getCurrentDisplayLayout();

  if (!layout) {
    return;
  }

  layout.remoteRect = {
    ...layoutDragState.startRect,
    x: Math.round(layoutDragState.startRect.x + dx),
    y: Math.round(layoutDragState.startRect.y + dy),
  };

  layout.switchEdge = inferSwitchEdge(layout.remoteRect);
  saveDisplayLayout(layout);
  renderDisplayLayoutEditor();
});

document.addEventListener("pointerup", () => {
  layoutDragState = null;
});

refreshTailscaleStatus().catch(renderTailscaleError);
// --- Quick start wiring ---
document.querySelector<HTMLButtonElement>("#qs-connect")?.addEventListener("click", async () => {
  const host = document.querySelector<HTMLInputElement>("#qs-host")!.value.trim();
  const status = document.querySelector<HTMLSpanElement>("#qs-status")!;
  if (!host) {
    status.textContent = "相手PCの Tailscale IP を入力してください。";
    return;
  }
  try {
    await invoke<TcpSessionSnapshot>("connect_tcp_peer", { host });
    // also mirror into the advanced TCP host field for consistency
    const adv = document.querySelector<HTMLInputElement>("#tcp-host");
    if (adv) adv.value = host;
    status.textContent = "接続要求を送信しました。";
    await refreshTcpSession();
  } catch (error) {
    status.textContent = `接続エラー: ${String(error)}`;
  }
});

// --- Receiver (make THIS PC controllable) ---
document.querySelector<HTMLButtonElement>("#qs-receiver")?.addEventListener("click", async () => {
  const state = document.querySelector<HTMLSpanElement>("#qs-receiver-state")!;
  try {
    const snap = await invoke<TcpSessionSnapshot>("start_tcp_receiver", {});
    state.textContent = snap.listening
      ? `受信中（${snap.listen_addr ?? "47110"}）。相手の接続を待っています。`
      : "受信を開始しました。";
    state.className = "qs-state qs-ok";
    await refreshTcpSession();
  } catch (error) {
    state.textContent = `受信開始エラー: ${String(error)}`;
    state.className = "qs-state qs-err";
  }
});

// --- KVM control (seamless edge-crossing; replaces the old "mirror") ---
const EDGE_LABEL: Record<string, string> = { top: "上", bottom: "下", left: "左", right: "右" };
type KvmEdge = "top" | "bottom" | "left" | "right";
// The peer (peer-pc) is pinned to one edge of one specific local monitor,
// identified by that monitor's physical-pixel rect so the backend can match it.
type PeerAttach = {
  rect: [number, number, number, number];
  edge: KvmEdge;
  // Peer screen's virtual rect (position + real resolution) for multi-edge
  // crossing. Optional for back-compat with values saved before this field.
  peerRect?: [number, number, number, number];
};
const PEER_ATTACH_KEY = "tailkvm.peerAttach.v1";

function getPeerAttach(): PeerAttach | null {
  try {
    const raw = localStorage.getItem(PEER_ATTACH_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as PeerAttach;
    if (
      Array.isArray(v.rect) &&
      v.rect.length === 4 &&
      v.rect.every((n) => Number.isFinite(n)) &&
      ["top", "bottom", "left", "right"].includes(v.edge)
    ) {
      return v;
    }
  } catch {
    // ignore malformed storage
  }
  return null;
}

function savePeerAttach(attach: PeerAttach) {
  localStorage.setItem(PEER_ATTACH_KEY, JSON.stringify(attach));
}

// Per-host cache of each peer's real virtual-screen size, learned from a live
// connection (get_peer_screen_size). Lets the position editor draw the remote
// at its true resolution even before/without a connection.
const PEER_SCREENS_KEY = "tailkvm.peerScreens.v1";
let lastPeerScreen: [number, number] | null = null;

function getPeerScreens(): Record<string, [number, number]> {
  try {
    const raw = localStorage.getItem(PEER_SCREENS_KEY);
    if (raw) return JSON.parse(raw) as Record<string, [number, number]>;
  } catch {
    // ignore malformed storage
  }
  return {};
}

function savePeerScreen(host: string, w: number, h: number) {
  if (!host || !(w > 0) || !(h > 0)) return;
  const all = getPeerScreens();
  all[host] = [w, h];
  localStorage.setItem(PEER_SCREENS_KEY, JSON.stringify(all));
  lastPeerScreen = [w, h];
}

function getPeerScreenForHost(host: string): [number, number] | null {
  const cached = host ? getPeerScreens()[host] : undefined;
  if (cached && cached[0] > 0 && cached[1] > 0) return cached;
  return lastPeerScreen;
}

function getKvmEdge(): KvmEdge {
  return getPeerAttach()?.edge ?? "bottom";
}

// KVM pointer-speed (gain): the backend scales raw mouse deltas by this so
// controlling the remote doesn't feel slow next to the local cursor.
const KVM_GAIN_KEY = "tailkvm.kvmGain";
function getKvmGain(): number {
  const fromInput = Number(document.querySelector<HTMLInputElement>("#qs-kvm-gain")?.value);
  const stored = Number(localStorage.getItem(KVM_GAIN_KEY));
  const g = fromInput || stored || 1.8;
  return Math.min(4, Math.max(0.5, g));
}
(() => {
  const range = document.querySelector<HTMLInputElement>("#qs-kvm-gain");
  const label = document.querySelector<HTMLElement>("#qs-kvm-gain-val");
  if (!range) return;
  const saved = Number(localStorage.getItem(KVM_GAIN_KEY));
  if (saved >= 0.5 && saved <= 4) range.value = String(saved);
  const sync = () => {
    if (label) label.textContent = `${Number(range.value).toFixed(1)}×`;
    localStorage.setItem(KVM_GAIN_KEY, range.value);
  };
  range.addEventListener("input", sync);
  sync();
})();

document.querySelector<HTMLButtonElement>("#qs-kvm-start")?.addEventListener("click", async () => {
  const status = document.querySelector<HTMLSpanElement>("#qs-status")!;
  const edge = getKvmEdge();
  const attach = getPeerAttach();
  // The backend maps the cursor onto the peer's real screen using the size the
  // peer reported via ScreenInfo, so we don't pass a guessed remote size here.
  // The attach rect pins crossing to the chosen monitor's edge (undefined = any).
  try {
    await invoke<TcpSessionSnapshot>("start_mouse_capture", {
      gain: getKvmGain(),
      intervalMs: 8,
      maxDelta: 80,
      remoteMode: true,
      seamless: true,
      switchEdge: edge,
      edgeMargin: 3,
      edgeDwellMs: 0,
      deadCornerPx: 0,
      attachLeft: attach?.rect[0],
      attachTop: attach?.rect[1],
      attachRight: attach?.rect[2],
      attachBottom: attach?.rect[3],
      peerLeft: attach?.peerRect?.[0],
      peerTop: attach?.peerRect?.[1],
      peerRight: attach?.peerRect?.[2],
      peerBottom: attach?.peerRect?.[3],
    });
    kvmActive = true;
    status.textContent = `KVM操作中: マウスを画面「${EDGE_LABEL[edge]}」端まで動かすと相手PCを操作。端で戻ると自分に戻ります。`;
    status.className = "qs-state qs-ok";
    await refreshTcpSession();
  } catch (error) {
    status.textContent = `開始できません: ${String(error)}（先に「接続」してください）`;
    status.className = "qs-state qs-err";
  }
});

// Emergency reset (#11): the strongest in-UI recovery — stops every forwarding
// path, force-releases the cursor clip, and aborts an inbound
// (being-controlled) session. Same action as the tray "Emergency reset" item.
document.querySelector<HTMLButtonElement>("#qs-emergency")?.addEventListener("click", async () => {
  const status = document.querySelector<HTMLSpanElement>("#qs-status")!;
  try {
    await invoke<TcpSessionSnapshot>("emergency_reset");
    kvmActive = false;
    status.textContent = "緊急リセット完了（全転送停止・カーソル解放・被制御切断）。";
    status.className = "qs-state";
    await refreshTcpSession();
  } catch (error) {
    status.textContent = `緊急リセット失敗: ${String(error)}`;
    status.className = "qs-state qs-err";
  }
});

document.querySelector<HTMLButtonElement>("#qs-kvm-stop")?.addEventListener("click", async () => {
  const status = document.querySelector<HTMLSpanElement>("#qs-status")!;
  try {
    await invoke<TcpSessionSnapshot>("stop_mouse_capture");
    kvmActive = false;
    status.textContent = "停止しました（自分の操作に戻りました）。";
    status.className = "qs-state";
    await refreshTcpSession();
  } catch (error) {
    status.textContent = `停止エラー: ${String(error)}`;
  }
});

// --- Status cards toggle (Runtime / Tailscale / Keyboard / Monitor / Peers) ---
document.querySelector<HTMLButtonElement>("#qs-toggle-status")?.addEventListener("click", () => {
  const on = document.body.classList.toggle("show-status");
  const btn = document.querySelector<HTMLButtonElement>("#qs-toggle-status");
  if (btn) {
    btn.textContent = on
      ? "状態カードを隠す ▲"
      : "状態（Runtime / Tailscale / Keyboard / モニタ / Peers）を表示 ▼";
  }
});

// --- Advanced settings toggle ---
document.querySelector<HTMLButtonElement>("#qs-toggle-advanced")?.addEventListener("click", () => {
  const on = document.body.classList.toggle("show-advanced");
  const btn = document.querySelector<HTMLButtonElement>("#qs-toggle-advanced");
  if (btn) {
    btn.textContent = on
      ? "詳細設定を隠す ▲"
      : "詳細設定（テスト/ルータ/Raw入力/クリップボード）を表示 ▼";
  }
});

// Initial data load. refreshMonitorTopology retries the monitor command
// internally with a timeout, so a transient/early failure recovers on its own
// instead of leaving the panel stuck on "読込中...".
refreshMonitorTopology().catch(renderMonitorError);
refreshTcpSession().catch(renderTcpError);
refreshLockState().catch(() => {});

setInterval(() => {
  refreshTcpSession().catch(renderTcpError);
  refreshLockState().catch(() => {});
}, 2000);

document.addEventListener("click", (event) => {
  const target = event.target;

  if (!(target instanceof HTMLElement)) {
    return;
  }

  const button = target.closest("button[data-peer-action][data-peer-ip]");

  if (!(button instanceof HTMLButtonElement)) {
    return;
  }

  const action = button.dataset.peerAction;
  const ip = button.dataset.peerIp ?? "";
  const host = button.dataset.peerHost ?? "";

  if (!ip) {
    return;
  }

  if (action === "connect") {
    setTextInputValue("#tcp-host", ip);
    renderTcpInfo(`Selected ${host || ip} for Connect peer: ${ip}`);
  }

  if (action === "firewall") {
    setTextInputValue("#firewall-remote", ip);
    renderTcpInfo(`Selected ${host || ip} for Firewall RemoteAddress: ${ip}`);
  }
});

async function sendTestMouseClick(button: "left" | "right" | "middle" | "x1" | "x2") {
  try {
    await invoke<TcpSessionSnapshot>("send_test_mouse_click", { button });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestKeyboardText(text: string) {
  try {
    await invoke<TcpSessionSnapshot>("send_test_keyboard_text", { text });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestKeyTap(key: string) {
  try {
    await invoke<TcpSessionSnapshot>("send_test_key_tap", { key });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestMouseDoubleClick(button: "left" | "right" | "middle" | "x1" | "x2") {
  try {
    await invoke<TcpSessionSnapshot>("send_test_mouse_double_click", { button });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function refreshTcpSession() {
  const state = await invoke<TcpSessionSnapshot>("get_tcp_session_state");
  renderTcpSession(state);
  updateQuickStartConn(state);

  // Learn the peer's real screen size while connected, cached per host so the
  // position editor can draw the remote at its true resolution.
  if (state.connected && state.peer_addr) {
    try {
      const size = await invoke<[number, number] | null>("get_peer_screen_size");
      if (size && size[0] > 0 && size[1] > 0) {
        savePeerScreen(state.peer_addr.replace(/:\d+$/, ""), size[0], size[1]);
      }
    } catch {
      // best-effort telemetry only
    }
  }
}

function renderTcpSession(state: TcpSessionSnapshot) {
  const summary = document.querySelector<HTMLParagraphElement>("#tcp-summary")!;
  const stateBox = document.querySelector<HTMLDivElement>("#tcp-state")!;

  const connectionText = state.connected ? "CONNECTED" : "DISCONNECTED";
  const listeningText = state.listening ? "LISTENING" : "NOT LISTENING";

  summary.textContent =
    `Role: ${state.role} / ${connectionText} / ${listeningText} / heartbeat seq=${state.heartbeat_seq}`;

  stateBox.classList.remove("empty");
  stateBox.innerHTML = `
    <section class="tcp-card">
      <div class="tcp-main">
        <div>
          <div class="tcp-title">
            TCP Session
            <span class="node-status ${state.connected ? "online" : "offline"}">${connectionText}</span>
            <span class="node-status ${state.listening ? "online" : "offline"}">${listeningText}</span>
          </div>
          <div class="tcp-subtitle">${escapeHtml(state.last_event)}</div>
        </div>
      </div>

      ${
        state.keyboard_layout_warning
          ? `<div class="error-box">⚠ ${escapeHtml(state.keyboard_layout_warning)}</div>`
          : ""
      }

      <dl class="tcp-meta">
        <div>
          <dt>Role</dt>
          <dd>${escapeHtml(state.role)}</dd>
        </div>
        <div>
          <dt>Listen addr</dt>
          <dd>${escapeHtml(state.listen_addr ?? "-")}</dd>
        </div>
        <div>
          <dt>Peer addr</dt>
          <dd>${escapeHtml(state.peer_addr ?? "-")}</dd>
        </div>
        <div>
          <dt>Peer name</dt>
          <dd>${escapeHtml(state.peer_name ?? "-")}</dd>
        </div>
        <div>
          <dt>Heartbeat</dt>
          <dd>${state.heartbeat_seq}</dd>
        </div>
        <div>
          <dt>Local layout</dt>
          <dd>${escapeHtml(state.local_keyboard_layout ?? "-")}</dd>
        </div>
        <div>
          <dt>Peer layout</dt>
          <dd>${escapeHtml(state.peer_keyboard_layout ?? "-")}</dd>
        </div>
        <div>
          <dt>IME mode</dt>
          <dd>${escapeHtml(state.ime_mode ?? "off")}</dd>
        </div>
      </dl>
    </section>
  `;

  // IME-UI-003/004: keep the IME section's status banner in sync.
  const imeStatus = document.querySelector<HTMLParagraphElement>("#ime-status");
  if (imeStatus) {
    const mode = state.ime_mode ?? "off";
    imeStatus.textContent =
      mode === "off" || mode === "suspended"
        ? `IME composition mode: ${mode}`
        : `IME composition mode: ${mode} — 変換はローカルIMEで行い、確定文字のみ相手PCへ送信します`;
  }
}

function renderTcpError(error: unknown) {
  const summary = document.querySelector<HTMLParagraphElement>("#tcp-summary")!;
  const stateBox = document.querySelector<HTMLDivElement>("#tcp-state")!;

  summary.textContent = "TCP session error.";
  stateBox.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
}

async function refreshTailscaleStatus() {
  const summary = document.querySelector<HTMLParagraphElement>("#tailscale-summary")!;
  const selfNode = document.querySelector<HTMLDivElement>("#self-node")!;
  const peerList = document.querySelector<HTMLDivElement>("#peer-list")!;

  summary.textContent = "Loading tailscale status...";
  selfNode.innerHTML = `<div class="empty">Loading...</div>`;
  peerList.innerHTML = `<div class="empty">Loading...</div>`;

  try {
    const status = await invoke<TailnetStatus>("get_tailscale_status");
    latestTailnetStatus = status;
    populateLayoutPeerSelect();
    renderDisplayLayoutEditor();

    const selfIpEl = document.querySelector<HTMLElement>("#qs-self-ip");
    const selfIp = status.self_node?.tailscale_ips?.[0];
    if (selfIpEl) {
      selfIpEl.textContent = selfIp ?? "(不明 — Tailscale 未接続?)";
    }
    const onlineCount = status.peers.filter((peer) => peer.online).length;

    // Mirror live telemetry into the header HUD.
    const hudSelf = document.querySelector<HTMLElement>("#hud-self");
    if (hudSelf) hudSelf.textContent = selfIp ?? "—";
    const hudPeers = document.querySelector<HTMLElement>("#hud-peers");
    if (hudPeers) hudPeers.textContent = String(onlineCount);

    summary.textContent = `Backend: ${status.backend_state} / Peers: ${onlineCount} online, ${status.raw_peer_count} total`;

    selfNode.classList.remove("empty");
    selfNode.innerHTML = status.self_node
      ? renderNodeCard(status.self_node, true)
      : `<div class="empty">Self node not found in tailscale status.</div>`;

    peerList.classList.remove("empty");
    peerList.innerHTML = status.peers.length > 0
      ? status.peers.map((peer) => renderNodeCard(peer, false)).join("")
      : `<div class="empty">No peers found.</div>`;
  } catch (error) {
    renderTailscaleError(error);
  }
}

async function refreshKeyboardLayout() {
  const summary = document.querySelector<HTMLParagraphElement>("#keyboard-layout-summary")!;
  summary.textContent = "Loading keyboard layout...";

  try {
    const info = await invoke<KeyboardLayoutInfo>("get_keyboard_layout");
    summary.textContent = info.label;
  } catch (error) {
    summary.textContent = `Keyboard layout error: ${String(error)}`;
  }
}

// Draw this PC's monitors to scale in the Quick Start card (always visible,
// no peer selection required).
/** Find the monitor whose physical rect matches a stored attach rect. */
function findMonitorByRect(rect: [number, number, number, number]): MonitorInfo | undefined {
  return latestMonitorTopology?.monitors.find(
    (m) =>
      m.rect_physical_px.left === rect[0] &&
      m.rect_physical_px.top === rect[1] &&
      m.rect_physical_px.right === rect[2] &&
      m.rect_physical_px.bottom === rect[3],
  );
}

/** Edges of `m` that face the outer boundary (no adjacent local monitor). Only
 * these are valid crossing edges — an interior edge would mean the cursor flows
 * into the neighbouring local monitor, not the remote. */
function outerEdgesOf(m: MonitorInfo, all: MonitorInfo[]): KvmEdge[] {
  const r = m.rect_physical_px;
  const tol = 2;
  const hasNeighbour = (edge: KvmEdge): boolean =>
    all.some((n) => {
      if (n === m) return false;
      const nr = n.rect_physical_px;
      if (edge === "bottom")
        return Math.abs(nr.top - r.bottom) <= tol && nr.left < r.right && nr.right > r.left;
      if (edge === "top")
        return Math.abs(nr.bottom - r.top) <= tol && nr.left < r.right && nr.right > r.left;
      if (edge === "right")
        return Math.abs(nr.left - r.right) <= tol && nr.top < r.bottom && nr.bottom > r.top;
      return Math.abs(nr.right - r.left) <= tol && nr.top < r.bottom && nr.bottom > r.top;
    });
  return (["left", "right", "top", "bottom"] as KvmEdge[]).filter((e) => !hasNeighbour(e));
}

// Interactive monitor map: shows this PC's real monitors and a draggable
// "相手PC" tile. Drop it next to a monitor edge to pin the peer there; the
// crossing then happens only at that monitor's that edge.
function renderQuickStartMonitors() {
  const box = document.querySelector<HTMLDivElement>("#qs-monitors");
  if (!box) return;
  const topo = latestMonitorTopology;
  if (!topo || topo.monitors.length === 0) {
    box.textContent = "モニター情報を取得できませんでした。";
    return;
  }
  const vs = topo.virtual_screen;
  const maxW = 560;
  const maxH = 220;
  const scale = Math.min(maxW / Math.max(1, vs.width), maxH / Math.max(1, vs.height), 0.25);
  const pad = 42; // room around the monitors so the peer tile can sit outside
  const w = Math.max(160, Math.round(vs.width * scale) + pad * 2);
  const h = Math.max(100, Math.round(vs.height * scale) + pad * 2);

  const toCanvas = (vx: number, vy: number) => ({
    x: Math.round((vx - vs.left) * scale) + pad,
    y: Math.round((vy - vs.top) * scale) + pad,
  });

  const monBoxes = topo.monitors
    .map((m) => {
      const r = m.rect_physical_px;
      const tl = toCanvas(r.left, r.top);
      const bw = Math.max(24, Math.round(r.width * scale));
      const bh = Math.max(18, Math.round(r.height * scale));
      const scalePct = Math.round((m.scale_factor || 1) * 100);
      return (
        `<div class="qs-mon${m.is_primary ? " qs-mon-primary" : ""}" ` +
        `style="left:${tl.x}px;top:${tl.y}px;width:${bw}px;height:${bh}px;" ` +
        `title="${escapeHtml(m.name)} ${r.width}x${r.height} @${scalePct}%">` +
        `<span>${r.width}×${r.height}<br/>${scalePct}%${m.is_primary ? " ★" : ""}</span>` +
        `</div>`
      );
    })
    .join("");

  // Current attach (or default: bottom edge of the primary monitor).
  const primary = topo.monitors.find((m) => m.is_primary) ?? topo.monitors[0];
  const stored = getPeerAttach();
  const am = (stored && findMonitorByRect(stored.rect)) ?? primary;
  const edge: KvmEdge = stored && findMonitorByRect(stored.rect) ? stored.edge : "bottom";
  const ar = am.rect_physical_px;

  // Peer resolution: draw the remote tile at the peer's real screen size, using
  // the same scale as the local monitors. Falls back to 1920x1080 until we have
  // learned the peer's size from a connection (cached per host).
  const curHost = (document.querySelector<HTMLInputElement>("#qs-host")?.value || "").trim();
  const peerRes = getPeerScreenForHost(curHost);
  const [pw, ph] = peerRes ?? [1920, 1080];

  // Place the peer tile just outside the attach edge of `am`, sized to (pw, ph).
  const tileW = Math.max(28, Math.round(pw * scale));
  const tileH = Math.max(20, Math.round(ph * scale));
  const gap = 6;
  const cTL = toCanvas(ar.left, ar.top);
  const monPxW = Math.max(24, Math.round(ar.width * scale));
  const monPxH = Math.max(18, Math.round(ar.height * scale));
  let px = cTL.x;
  let py = cTL.y;
  if (edge === "bottom") {
    px = cTL.x + monPxW / 2 - tileW / 2;
    py = cTL.y + monPxH + gap;
  } else if (edge === "top") {
    px = cTL.x + monPxW / 2 - tileW / 2;
    py = cTL.y - tileH - gap;
  } else if (edge === "left") {
    px = cTL.x - tileW - gap;
    py = cTL.y + monPxH / 2 - tileH / 2;
  } else {
    px = cTL.x + monPxW + gap;
    py = cTL.y + monPxH / 2 - tileH / 2;
  }

  // If a peer rect was stored from a previous drag, position the tile from it so
  // the visual matches the backend's multi-edge crossing geometry.
  const storedRect = stored?.peerRect;
  if (storedRect) {
    const ptl = toCanvas(storedRect[0], storedRect[1]);
    px = ptl.x;
    py = ptl.y;
  }

  // Connection-candidate list (online Tailnet peers) shown to the right of the
  // virtual-screen map. Clicking a row fills the host field for step 01.
  // (curHost is computed above for the peer-resolution lookup.)
  const cands = (latestTailnetStatus?.peers ?? [])
    .map((p) => ({ name: p.host_name, ip: getPrimaryTailscaleIp(p), online: !!p.online }))
    .filter((p): p is { name: string; ip: string; online: boolean } => !!p.ip)
    .sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name));
  const candItems = cands.length
    ? cands
        .map((p) => {
          const sel = p.ip === curHost ? " is-selected" : "";
          return (
            `<button type="button" class="qs-cand${sel}" data-ip="${escapeHtml(p.ip)}" ` +
            `title="${escapeHtml(p.name)} / ${escapeHtml(p.ip)}">` +
            `<i class="qs-cand-lamp ${p.online ? "on" : "off"}"></i>` +
            `<span class="qs-cand-name">${escapeHtml(p.name)}</span>` +
            `<span class="qs-cand-ip">${escapeHtml(p.ip)}</span></button>`
          );
        })
        .join("")
    : `<div class="empty">接続候補なし<br/>「状態」→ Refresh peers</div>`;

  // Which monitor edges the placed peer rect is flush against (mirrors the
  // backend peer_adjacent), so the user can confirm a corner touches two
  // monitors and crosses on both.
  let crossLabel = "";
  if (storedRect) {
    const TOL = 6;
    const parts: string[] = [];
    for (const m of topo.monitors) {
      const r = m.rect_physical_px;
      const xov = Math.min(r.right, storedRect[2]) - Math.max(r.left, storedRect[0]);
      const yov = Math.min(r.bottom, storedRect[3]) - Math.max(r.top, storedRect[1]);
      const short = m.name.split("\\").pop() || m.name;
      const checks: Array<[boolean, KvmEdge]> = [
        [Math.abs(storedRect[1] - r.bottom) <= TOL && xov > 0, "bottom"],
        [Math.abs(r.top - storedRect[3]) <= TOL && xov > 0, "top"],
        [Math.abs(storedRect[0] - r.right) <= TOL && yov > 0, "right"],
        [Math.abs(r.left - storedRect[2]) <= TOL && yov > 0, "left"],
      ];
      for (const [hit, e] of checks) {
        if (hit) parts.push(`${short} ${EDGE_LABEL[e]}端`);
      }
    }
    crossLabel = parts.join(" ／ ");
  }

  box.innerHTML =
    `<div class="qs-mon-layout">` +
    `<div class="qs-mon-left">` +
    `<div id="qs-mon-canvas" class="qs-mon-canvas" style="width:${w}px;height:${h}px;">` +
    monBoxes +
    `<div id="qs-peer-tile" class="qs-peer-tile" ` +
    `style="left:${px}px;top:${py}px;width:${tileW}px;height:${tileH}px;" ` +
    `title="相手PC ${pw}×${ph}${peerRes ? "" : "（推定 — 接続後に実寸へ）"}">` +
    `相手PC${peerRes ? `<br><small>${pw}×${ph}</small>` : ""}</div>` +
    `</div>` +
    (storedRect
      ? `<div class="qs-cross-edges">越境辺: ${crossLabel || "なし（タイルをモニタの角へ寄せて）"}</div>`
      : "") +
    `</div>` +
    `<aside class="qs-peer-list">` +
    `<div class="qs-peer-list-head">接続候補 / PEERS</div>` +
    `<div class="qs-peer-list-body">${candItems}</div>` +
    `</aside>` +
    `</div>`;

  // Click a candidate -> load it into the host field and mark it selected.
  box.querySelectorAll<HTMLButtonElement>(".qs-cand").forEach((btn) => {
    btn.addEventListener("click", () => {
      const ip = btn.dataset.ip || "";
      const host = document.querySelector<HTMLInputElement>("#qs-host");
      if (host) host.value = ip;
      box.querySelectorAll(".qs-cand").forEach((b) => b.classList.remove("is-selected"));
      btn.classList.add("is-selected");
      document.querySelector<HTMLButtonElement>("#qs-connect")?.focus();
    });
  });

  const canvas = document.querySelector<HTMLDivElement>("#qs-mon-canvas");
  const tile = document.querySelector<HTMLDivElement>("#qs-peer-tile");
  if (!canvas || !tile) return;

  let dragging = false;
  tile.addEventListener("pointerdown", (ev) => {
    dragging = true;
    tile.setPointerCapture(ev.pointerId);
    tile.classList.add("dragging");
    ev.preventDefault();
  });
  tile.addEventListener("pointermove", (ev) => {
    if (!dragging) return;
    const rect = canvas.getBoundingClientRect();
    tile.style.left = `${ev.clientX - rect.left - tileW / 2}px`;
    tile.style.top = `${ev.clientY - rect.top - tileH / 2}px`;
  });
  tile.addEventListener("pointerup", (ev) => {
    if (!dragging) return;
    dragging = false;
    tile.classList.remove("dragging");
    tile.releasePointerCapture(ev.pointerId);
    const rect = canvas.getBoundingClientRect();
    // Drop point in virtual-desktop coordinates.
    const vx = (ev.clientX - rect.left - pad) / scale + vs.left;
    const vy = (ev.clientY - rect.top - pad) / scale + vs.top;
    // Nearest monitor (squared distance from the point to its rect).
    const distSq = (m: MonitorInfo) => {
      const r = m.rect_physical_px;
      const ddx = Math.max(r.left - vx, 0, vx - r.right);
      const ddy = Math.max(r.top - vy, 0, vy - r.bottom);
      return ddx * ddx + ddy * ddy;
    };
    const target = [...topo.monitors].sort((a, b) => distSq(a) - distSq(b))[0];
    const tr = target.rect_physical_px;
    // Only outer edges (no adjacent local monitor) are valid — otherwise the
    // cursor would flow into the neighbour, not the remote. Snap to the nearest
    // valid edge of the chosen monitor.
    const d: Record<KvmEdge, number> = {
      left: Math.abs(vx - tr.left),
      right: Math.abs(vx - tr.right),
      top: Math.abs(vy - tr.top),
      bottom: Math.abs(vy - tr.bottom),
    };
    const valid = outerEdgesOf(target, topo.monitors);
    const candidates: KvmEdge[] = valid.length ? valid : ["left", "right", "top", "bottom"];
    const dropped = candidates.reduce((best, e) => (d[e] < d[best] ? e : best), candidates[0]);
    // Peer's virtual rect: flush against the dropped edge, slid to the drop
    // point (clamped to keep meaningful overlap with the target monitor). This
    // lets the peer be parked at a corner so it touches two monitors — the
    // backend then crosses on both the vertical and the horizontal edge.
    // Place the peer flush against the dropped edge, slid to the drop point, then
    // SNAP the perpendicular side to a nearby monitor edge (that shares overlap on
    // the common axis) so parking it near a corner makes it flush with a SECOND
    // monitor too — the backend then crosses on both the vertical and the
    // horizontal edge. Only monitors that overlap the peer on the shared axis are
    // snap candidates (the target itself only touches as a line, so it is skipped).
    const clampN = (n: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, n));
    let pr: [number, number, number, number];
    if (dropped === "bottom" || dropped === "top") {
      const topY = dropped === "bottom" ? tr.bottom : tr.top - ph;
      const botY = topY + ph;
      const ov = Math.max(40, Math.round(Math.min(pw, tr.width) * 0.3));
      let left = Math.round(clampN(vx - pw / 2, tr.left - (pw - ov), tr.right - ov));
      let best: number | null = null;
      let bestD = Math.max(160, Math.round(pw * 0.6)); // snap radius
      for (const m of topo.monitors) {
        const r = m.rect_physical_px;
        if (Math.min(botY, r.bottom) - Math.max(topY, r.top) <= 0) continue; // need y-overlap
        for (const cand of [r.right, r.left - pw]) {
          const dd = Math.abs(cand - left);
          if (dd < bestD) {
            bestD = dd;
            best = cand;
          }
        }
      }
      if (best !== null) left = best;
      pr = [left, topY, left + pw, botY];
    } else {
      const leftX = dropped === "right" ? tr.right : tr.left - pw;
      const rightX = leftX + pw;
      const ov = Math.max(40, Math.round(Math.min(ph, tr.height) * 0.3));
      let top = Math.round(clampN(vy - ph / 2, tr.top - (ph - ov), tr.bottom - ov));
      let best: number | null = null;
      let bestD = Math.max(160, Math.round(ph * 0.6));
      for (const m of topo.monitors) {
        const r = m.rect_physical_px;
        if (Math.min(rightX, r.right) - Math.max(leftX, r.left) <= 0) continue; // need x-overlap
        for (const cand of [r.bottom, r.top - ph]) {
          const dd = Math.abs(cand - top);
          if (dd < bestD) {
            bestD = dd;
            best = cand;
          }
        }
      }
      if (best !== null) top = best;
      pr = [leftX, top, rightX, top + ph];
    }
    savePeerAttach({ rect: [tr.left, tr.top, tr.right, tr.bottom], edge: dropped, peerRect: pr });
    renderQuickStartMonitors();
  });
}

function updateQuickStartConn(snapshot: TcpSessionSnapshot) {
  const el = document.querySelector<HTMLSpanElement>("#qs-conn");
  if (!el) return;
  if (snapshot.connected) {
    const who = snapshot.peer_name || snapshot.peer_addr || "peer";
    el.textContent = `接続中: ${who}`;
    el.className = "qs-state qs-ok";
  } else if (snapshot.peer_addr) {
    // A connection was attempted but is not established — surface the reason
    // (connection refused = receiver not listening / firewall blocking).
    const reason = /fail|refus|timed|error|closed|disconnect/i.test(snapshot.last_event)
      ? snapshot.last_event
      : "未接続";
    el.textContent = `未接続 — ${reason}`;
    el.className = "qs-state qs-err";
  } else {
    el.textContent = "未接続";
    el.className = "qs-state";
  }

  // Flow guidance: light the active step, pulse the next action, mark the
  // connect step done, and flash once on a fresh connection.
  const connectStep = document.querySelector<HTMLElement>('.qs-row[data-step="01"]');
  const controlStep = document.querySelector<HTMLElement>('.qs-kvm[data-step="03"]');
  connectStep?.classList.toggle("is-active", !snapshot.connected);
  connectStep?.classList.toggle("is-done", snapshot.connected);
  controlStep?.classList.toggle("is-active", snapshot.connected);
  document
    .querySelector<HTMLButtonElement>("#qs-connect")
    ?.classList.toggle("is-next", !snapshot.connected);
  document
    .querySelector<HTMLButtonElement>("#qs-kvm-start")
    ?.classList.toggle("is-next", snapshot.connected && !kvmActive);

  if (snapshot.connected && !wasConnected && connectStep) {
    connectStep.classList.remove("flash-ok");
    void connectStep.offsetWidth; // reflow so the keyframe restarts
    connectStep.classList.add("flash-ok");
  }
  wasConnected = snapshot.connected;

  const hudLink = document.querySelector<HTMLElement>("#hud-link");
  if (hudLink) {
    hudLink.innerHTML = snapshot.connected
      ? `<i class="hud-lamp ok"></i>LINKED`
      : `<i class="hud-lamp"></i>OFFLINE`;
    hudLink.title = snapshot.connected ? snapshot.peer_name || snapshot.peer_addr || "" : "";
  }
}

async function refreshMonitorTopology() {
  const summary = document.querySelector<HTMLParagraphElement>("#monitor-summary")!;
  const list = document.querySelector<HTMLDivElement>("#monitor-list")!;

  summary.textContent = "Loading monitor topology...";
  list.innerHTML = `<div class="empty">Loading...</div>`;

  try {
    const topology = await withRetry(() =>
      withTimeout(
        invoke<MonitorTopology>("get_windows_monitor_topology"),
        4000,
        "get_windows_monitor_topology",
      ),
    );
    latestMonitorTopology = topology;
    renderDisplayLayoutEditor();
    renderQuickStartMonitors();
    const virtual = topology.virtual_screen;

    summary.textContent =
      `Virtual screen: ${formatRect(virtual)} / Monitors: ${topology.monitors.length}`;

    list.classList.remove("empty");
    list.innerHTML = `
      <section class="virtual-screen-card">
        <div class="monitor-title">Virtual Screen</div>
        <div class="monitor-rect">${escapeHtml(formatRect(virtual))}</div>
        <div class="monitor-note">
          Negative left/top values mean at least one monitor is placed left or above the primary monitor.
        </div>
      </section>
      ${topology.monitors.map(renderMonitorCard).join("")}
    `;
  } catch (error) {
    renderMonitorError(error);
  }
}


function populateLayoutPeerSelect() {
  const select = document.querySelector<HTMLSelectElement>("#layout-peer");

  if (!select || !latestTailnetStatus) {
    return;
  }

  const saved = loadDisplayLayout();
  const current = select.value || saved?.targetPeerIp || "";

  const peers = latestTailnetStatus.peers
    .map((peer) => {
      const ip = getPrimaryTailscaleIp(peer);
      return { peer, ip };
    })
    .filter((item) => !!item.ip);

  select.innerHTML = `<option value="">Select peer...</option>` +
    peers
      .map(({ peer, ip }) => {
        const selected = ip === current ? "selected" : "";
        return `<option value="${escapeHtml(ip)}" data-host="${escapeHtml(peer.host_name)}" ${selected}>${escapeHtml(peer.host_name)} / ${escapeHtml(ip)}</option>`;
      })
      .join("");
}

function renderDisplayLayoutEditor() {
  const summary = document.querySelector<HTMLParagraphElement>("#layout-summary");
  const canvas = document.querySelector<HTMLDivElement>("#layout-canvas");

  if (!summary || !canvas) {
    return;
  }

  populateLayoutPeerSelect();

  if (!latestMonitorTopology) {
    summary.textContent = "Monitor topology is not loaded yet.";
    canvas.className = "layout-canvas empty";
    canvas.textContent = "Refresh monitors first.";
    return;
  }

  const layout = getCurrentDisplayLayout();

  if (!layout) {
    summary.textContent = "Select a target peer to arrange the remote display.";
    canvas.className = "layout-canvas empty";
    canvas.textContent = "Select a target peer.";
    return;
  }

  layout.switchEdge = inferSwitchEdge(layout.remoteRect);
  saveDisplayLayout(layout);

  const localVirtual = latestMonitorTopology.virtual_screen;
  const scale = getLayoutScale();
  const padding = 40;

  const bounds = unionRects([
    {
      x: localVirtual.left,
      y: localVirtual.top,
      width: localVirtual.width,
      height: localVirtual.height,
    },
    layout.remoteRect,
  ]);

  const canvasWidth = Math.max(680, Math.round(bounds.width * scale + padding * 2));
  const canvasHeight = Math.max(300, Math.round(bounds.height * scale + padding * 2));

  canvas.className = "layout-canvas";
  canvas.style.width = `${canvasWidth}px`;
  canvas.style.height = `${canvasHeight}px`;

  const monitorHtml = latestMonitorTopology.monitors
    .map((monitor) => {
      const rect = monitor.rect_physical_px;
      const style = layoutRectStyle(
        {
          x: rect.left,
          y: rect.top,
          width: rect.width,
          height: rect.height,
        },
        bounds,
        scale,
        padding,
      );

      return `
        <div class="layout-monitor local ${monitor.is_primary ? "primary" : ""}" style="${style}">
          <div class="layout-monitor-title">${escapeHtml(monitor.name)}</div>
          <div class="layout-monitor-subtitle">${monitor.rect_physical_px.width} x ${monitor.rect_physical_px.height}</div>
          ${monitor.is_primary ? `<div class="layout-monitor-badge">PRIMARY</div>` : ""}
        </div>
      `;
    })
    .join("");

  const remoteStyle = layoutRectStyle(layout.remoteRect, bounds, scale, padding);

  canvas.innerHTML = `
    ${monitorHtml}
    <div class="layout-monitor remote layout-remote" style="${remoteStyle}">
      <div class="layout-monitor-title">${escapeHtml(layout.targetPeerHost || "Remote peer")}</div>
      <div class="layout-monitor-subtitle">${Math.round(layout.remoteRect.width)} x ${Math.round(layout.remoteRect.height)}</div>
      <div class="layout-monitor-badge">REMOTE</div>
      <div class="layout-drag-hint">drag</div>
    </div>
  `;

  summary.textContent =
    `Target: ${layout.targetPeerHost || layout.targetPeerIp} / IP: ${layout.targetPeerIp} / inferred switch edge: ${layout.switchEdge}`;
}

function getCurrentDisplayLayout(): SavedDisplayLayout | null {
  const select = document.querySelector<HTMLSelectElement>("#layout-peer");
  const selectedOption = select?.selectedOptions.item(0);
  const selectedIp = select?.value || "";
  const selectedHost = selectedOption?.dataset.host || selectedOption?.textContent?.split("/")[0]?.trim() || "";

  if (!selectedIp) {
    return null;
  }

  const saved = loadDisplayLayout();

  if (saved && saved.targetPeerIp === selectedIp) {
    const remoteWidth = getNumberInput("#layout-remote-width", Math.round(saved.remoteRect.width));
    const remoteHeight = getNumberInput("#layout-remote-height", Math.round(saved.remoteRect.height));

    saved.remoteRect.width = remoteWidth;
    saved.remoteRect.height = remoteHeight;
    saved.targetPeerHost = selectedHost || saved.targetPeerHost;
    return saved;
  }

  if (!latestMonitorTopology) {
    return null;
  }

  const virtual = latestMonitorTopology.virtual_screen;
  const remoteWidth = getNumberInput("#layout-remote-width", 1920);
  const remoteHeight = getNumberInput("#layout-remote-height", 1080);

  return {
    targetPeerIp: selectedIp,
    targetPeerHost: selectedHost,
    remoteRect: {
      x: virtual.right + 120,
      y: virtual.top,
      width: remoteWidth,
      height: remoteHeight,
    },
    switchEdge: "right",
  };
}

function applyDisplayLayoutToControls() {
  const layout = getCurrentDisplayLayout();

  if (!layout) {
    renderTcpInfo("Select a target peer in Display Layout Editor first.");
    return;
  }

  layout.switchEdge = inferSwitchEdge(layout.remoteRect);
  saveDisplayLayout(layout);

  setTextInputValue("#tcp-host", layout.targetPeerIp);
  setTextInputValue("#firewall-remote", layout.targetPeerIp);

  const switchEdge = document.querySelector<HTMLSelectElement>("#switch-edge");

  if (switchEdge) {
    switchEdge.value = layout.switchEdge;
  }

  renderTcpInfo(
    `Applied display layout.\nConnect peer: ${layout.targetPeerIp}\nFirewall remote: ${layout.targetPeerIp}\nSwitch edge: ${layout.switchEdge}`,
  );
}

function updateSavedRemoteSizeFromInputs() {
  const layout = getCurrentDisplayLayout();

  if (!layout) {
    return;
  }

  layout.remoteRect.width = getNumberInput("#layout-remote-width", Math.round(layout.remoteRect.width));
  layout.remoteRect.height = getNumberInput("#layout-remote-height", Math.round(layout.remoteRect.height));
  layout.switchEdge = inferSwitchEdge(layout.remoteRect);
  saveDisplayLayout(layout);
}

function loadDisplayLayout(): SavedDisplayLayout | null {
  try {
    const raw = localStorage.getItem(LAYOUT_STORAGE_KEY);

    if (!raw) {
      return null;
    }

    return JSON.parse(raw) as SavedDisplayLayout;
  } catch {
    return null;
  }
}

function saveDisplayLayout(layout: SavedDisplayLayout) {
  localStorage.setItem(LAYOUT_STORAGE_KEY, JSON.stringify(layout));
}

function getLayoutScale(): number {
  return Math.max(0.03, Math.min(0.4, getFloatInput("#layout-scale", 0.12)));
}

function getPrimaryTailscaleIp(node: TailnetNode): string {
  return node.tailscale_ips.find((value) => value.includes(".")) ?? node.tailscale_ips[0] ?? "";
}

function inferSwitchEdge(remoteRect: LayoutRect): "left" | "right" | "top" | "bottom" {
  if (!latestMonitorTopology) {
    return "right";
  }

  const local = latestMonitorTopology.virtual_screen;
  const remoteCenterX = remoteRect.x + remoteRect.width / 2;
  const remoteCenterY = remoteRect.y + remoteRect.height / 2;
  const localCenterX = local.left + local.width / 2;
  const localCenterY = local.top + local.height / 2;

  const dx = remoteCenterX - localCenterX;
  const dy = remoteCenterY - localCenterY;

  if (Math.abs(dx) >= Math.abs(dy)) {
    return dx >= 0 ? "right" : "left";
  }

  return dy >= 0 ? "bottom" : "top";
}

function unionRects(rects: LayoutRect): LayoutRect;
function unionRects(rects: LayoutRect[]): LayoutRect;
function unionRects(rects: LayoutRect | LayoutRect[]): LayoutRect {
  const items = Array.isArray(rects) ? rects : [rects];

  const left = Math.min(...items.map((rect) => rect.x));
  const top = Math.min(...items.map((rect) => rect.y));
  const right = Math.max(...items.map((rect) => rect.x + rect.width));
  const bottom = Math.max(...items.map((rect) => rect.y + rect.height));

  return {
    x: left,
    y: top,
    width: right - left,
    height: bottom - top,
  };
}

function layoutRectStyle(rect: LayoutRect, bounds: LayoutRect, scale: number, padding: number): string {
  const left = Math.round((rect.x - bounds.x) * scale + padding);
  const top = Math.round((rect.y - bounds.y) * scale + padding);
  const width = Math.max(48, Math.round(rect.width * scale));
  const height = Math.max(36, Math.round(rect.height * scale));

  return `left:${left}px; top:${top}px; width:${width}px; height:${height}px;`;
}

function renderTailscaleError(error: unknown) {
  const summary = document.querySelector<HTMLParagraphElement>("#tailscale-summary")!;
  const selfNode = document.querySelector<HTMLDivElement>("#self-node")!;
  const peerList = document.querySelector<HTMLDivElement>("#peer-list")!;

  summary.textContent = "Failed to load tailscale status.";
  selfNode.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
  peerList.innerHTML = `<div class="empty">Fix the error above, then refresh.</div>`;
}

function renderMonitorError(error: unknown) {
  const summary = document.querySelector<HTMLParagraphElement>("#monitor-summary")!;
  const list = document.querySelector<HTMLDivElement>("#monitor-list")!;

  summary.textContent = "Failed to load monitor topology.";
  list.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;

  // Also surface the failure in the Quick Start panel; otherwise it stays stuck
  // on "読込中..." indefinitely and looks like a hang rather than an error.
  const qs = document.querySelector<HTMLDivElement>("#qs-monitors");
  if (qs) {
    qs.textContent = `モニター情報を取得できませんでした: ${String(error)}`;
  }
}

function renderNodeCard(node: TailnetNode, isSelf: boolean): string {
  const ip = node.tailscale_ips.find((value) => value.includes(".")) ?? node.tailscale_ips[0] ?? "-";
  const dns = node.dns_name ?? "-";
  const os = node.os ?? "-";
  const user = node.user ?? "-";
  const relay = node.relay ?? "-";
  const lastSeen =
    !node.last_seen || node.last_seen.startsWith("0001-01-01")
      ? "-"
      : node.last_seen;
  const statusClass = node.online ? "online" : "offline";
  const statusText = node.online ? "ONLINE" : "OFFLINE";

  const peerActions =
    !isSelf && ip !== "-"
      ? `
        <div class="peer-actions">
          <button
            class="secondary-button"
            data-peer-action="connect"
            data-peer-ip="${escapeHtml(ip)}"
            data-peer-host="${escapeHtml(node.host_name)}"
          >
            Use for Connect
          </button>

          <button
            class="secondary-button"
            data-peer-action="firewall"
            data-peer-ip="${escapeHtml(ip)}"
            data-peer-host="${escapeHtml(node.host_name)}"
          >
            Use for Firewall
          </button>
        </div>
      `
      : "";

  return `
    <section class="peer-card ${isSelf ? "self" : ""}">
      <div class="peer-main">
        <div>
          <div class="peer-title">${escapeHtml(node.host_name)} ${isSelf ? `<span class="self-badge">SELF</span>` : ""}</div>
          <div class="peer-subtitle">${escapeHtml(dns)}</div>
        </div>
        <span class="node-status ${statusClass}">${statusText}</span>
      </div>

      <dl class="peer-meta">
        <div>
          <dt>Tailscale IP</dt>
          <dd>${escapeHtml(ip)}</dd>
        </div>
        <div>
          <dt>OS</dt>
          <dd>${escapeHtml(os)}</dd>
        </div>
        <div>
          <dt>User</dt>
          <dd>${escapeHtml(user)}</dd>
        </div>
        <div>
          <dt>Relay</dt>
          <dd>${escapeHtml(relay)}</dd>
        </div>
        <div>
          <dt>Last seen</dt>
          <dd>${escapeHtml(lastSeen)}</dd>
        </div>
      </dl>

      ${peerActions}
    </section>
  `;
}

function renderMonitorCard(monitor: MonitorInfo): string {
  const scalePercent = `${Math.round(monitor.scale_factor * 100)}%`;

  return `
    <section class="monitor-card">
      <div class="monitor-main">
        <div>
          <div class="monitor-title">
            ${escapeHtml(monitor.name)}
            ${monitor.is_primary ? `<span class="primary-badge">PRIMARY</span>` : ""}
          </div>
          <div class="monitor-subtitle">${escapeHtml(monitor.id)}</div>
        </div>
        <span class="dpi-badge">${monitor.dpi_x} DPI / ${scalePercent}</span>
      </div>

      <dl class="monitor-meta">
        <div>
          <dt>Monitor rect</dt>
          <dd>${escapeHtml(formatRect(monitor.rect_physical_px))}</dd>
        </div>
        <div>
          <dt>Work area</dt>
          <dd>${escapeHtml(formatRect(monitor.work_area_physical_px))}</dd>
        </div>
        <div>
          <dt>Size</dt>
          <dd>${monitor.rect_physical_px.width} x ${monitor.rect_physical_px.height}px</dd>
        </div>
        <div>
          <dt>DPI</dt>
          <dd>${monitor.dpi_x} x ${monitor.dpi_y}</dd>
        </div>
      </dl>
    </section>
  `;
}

function getSelectedRemoteSize(): { width: number; height: number } {
  const layout = getCurrentDisplayLayout();

  if (layout) {
    return {
      width: Math.round(layout.remoteRect.width),
      height: Math.round(layout.remoteRect.height),
    };
  }

  return {
    width: getNumberInput("#layout-remote-width", 1920),
    height: getNumberInput("#layout-remote-height", 1080),
  };
}

function getFloatInput(selector: string, fallback: number): number {
  const input = document.querySelector<HTMLInputElement>(selector);
  const value = Number(input?.value.trim() ?? "");

  if (!Number.isFinite(value)) {
    return fallback;
  }

  return value;
}

function getNumberInput(selector: string, fallback: number): number {
  const input = document.querySelector<HTMLInputElement>(selector)!;
  const value = Number(input.value.trim());

  if (!Number.isFinite(value)) {
    return fallback;
  }

  return Math.trunc(value);
}

function setTextInputValue(selector: string, value: string) {
  const input = document.querySelector<HTMLInputElement>(selector);

  if (input) {
    input.value = value;
  }
}

function renderTcpInfo(message: string) {
  const summary = document.querySelector<HTMLParagraphElement>("#tcp-summary")!;
  const stateBox = document.querySelector<HTMLDivElement>("#tcp-state")!;

  summary.textContent = message;
  stateBox.innerHTML = `<div class="info-box">${escapeHtml(message)}</div>`;
}

function getPortValue(): number {
  const input = document.querySelector<HTMLInputElement>("#tcp-port")!;
  const port = Number(input.value.trim() || DEFAULT_PORT);

  if (!Number.isFinite(port) || port < 1 || port > 65535) {
    return DEFAULT_PORT;
  }

  return Math.trunc(port);
}

function formatRect(rect: RectI32): string {
  return `left=${rect.left}, top=${rect.top}, right=${rect.right}, bottom=${rect.bottom}, size=${rect.width}x${rect.height}`;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}
