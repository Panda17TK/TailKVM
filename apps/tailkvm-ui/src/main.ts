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
};

const DEFAULT_PORT = 47110;
const LAYOUT_STORAGE_KEY = "tailkvm.displayLayout.v1";

let latestTailnetStatus: TailnetStatus | null = null;
let latestMonitorTopology: MonitorTopology | null = null;

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
          Task 4: TCP session over Tailscale with Hello and Heartbeat.
        </p>
      </div>
      <div class="status-pill">TRAY READY</div>
    </section>

    <section class="grid">
      <article class="card">
        <h2>Runtime</h2>
        <p id="runtime-status">Not checked yet.</p>
        <button id="check-status">Check Rust backend</button>
      </article>

      <article class="card">
        <h2>Tailscale</h2>
        <p id="tailscale-summary">Not loaded yet.</p>
        <button id="refresh-tailscale">Refresh peers</button>
      </article>

      <article class="card full">
        <h2>TCP Session</h2>
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
          <button id="refresh-tcp">Refresh TCP state</button>

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
            <input id="capture-interval-ms" type="number" value="33" min="8" max="100" />
          </label>

          <label>
            Max delta
            <input id="max-delta" type="number" value="80" min="10" max="500" />

          </label>

                    <label class="checkbox-label">
            <input id="remote-mode" type="checkbox" checked />
            Remote mode
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
        </div>

        <div id="tcp-state" class="tcp-state empty">Not loaded yet.</div>
      </article>

      <article class="card full">
        <h2>Display Layout Editor</h2>
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

      <article class="card full">
        <h2>Monitor Topology</h2>
        <p id="monitor-summary">Not loaded yet.</p>
        <button id="refresh-monitors">Refresh monitors</button>
        <div id="monitor-list" class="monitor-list empty">Not loaded yet.</div>
      </article>

      <article class="card full">
        <h2>This machine</h2>
        <div id="self-node" class="empty">Not loaded yet.</div>
      </article>

      <article class="card full">
        <h2>Peers</h2>
        <div id="peer-list" class="peer-list empty">Not loaded yet.</div>
      </article>
    </section>
  </main>
`;

document
  .querySelector<HTMLButtonElement>("#check-status")!
  .addEventListener("click", async () => {
    const status = await invoke<string>("get_app_status");
    document.querySelector<HTMLParagraphElement>("#runtime-status")!.textContent = status;
  });

document
  .querySelector<HTMLButtonElement>("#refresh-tailscale")!
  .addEventListener("click", async () => refreshTailscaleStatus());

document
  .querySelector<HTMLButtonElement>("#refresh-monitors")!
  .addEventListener("click", async () => refreshMonitorTopology());

document
  .querySelector<HTMLButtonElement>("#refresh-tcp")!
  .addEventListener("click", async () => refreshTcpSession());

document
  .querySelector<HTMLButtonElement>("#install-firewall")!
  .addEventListener("click", async () => {
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
  .querySelector<HTMLButtonElement>("#send-mouse-test")!
  .addEventListener("click", async () => {
    const dx = getNumberInput("#mouse-dx", 80);
    const dy = getNumberInput("#mouse-dy", 0);

    await invoke<TcpSessionSnapshot>("send_test_mouse_move", { dx, dy });
    await refreshTcpSession();
  });

document
  .querySelector<HTMLButtonElement>("#start-receiver")!
  .addEventListener("click", async () => {
    const port = getPortValue();
    await invoke<TcpSessionSnapshot>("start_tcp_receiver", { port });
    await refreshTcpSession();
  });

document
  .querySelector<HTMLButtonElement>("#connect-peer")!
  .addEventListener("click", async () => {
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
  .querySelector<HTMLButtonElement>("#send-left-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseClick("left");
  });

document
  .querySelector<HTMLButtonElement>("#send-right-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseClick("right");
  });

document
  .querySelector<HTMLButtonElement>("#send-middle-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseClick("middle");
  });

document
  .querySelector<HTMLButtonElement>("#send-x1-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseClick("x1");
  });

document
  .querySelector<HTMLButtonElement>("#send-x2-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseClick("x2");
  });

document
  .querySelector<HTMLButtonElement>("#send-left-double-click-test")!
  .addEventListener("click", async () => {
    await sendTestMouseDoubleClick("left");
  });

document
  .querySelector<HTMLButtonElement>("#start-mouse-hook-capture")!
  .addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("start_mouse_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-mouse-hook-capture")!
  .addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_mouse_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#send-keyboard-text")!
  .addEventListener("click", async () => {
    const text = document.querySelector<HTMLInputElement>("#keyboard-text")!.value;
    await sendTestKeyboardText(text);
  });

document
  .querySelector<HTMLButtonElement>("#send-key-enter")!
  .addEventListener("click", async () => {
    await sendTestKeyTap("enter");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-backspace")!
  .addEventListener("click", async () => {
    await sendTestKeyTap("backspace");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-tab")!
  .addEventListener("click", async () => {
    await sendTestKeyTap("tab");
  });

document
  .querySelector<HTMLButtonElement>("#send-key-escape")!
  .addEventListener("click", async () => {
    await sendTestKeyTap("escape");
  });

document
  .querySelector<HTMLButtonElement>("#start-keyboard-hook-capture")!
  .addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("start_keyboard_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-keyboard-hook-capture")!
  .addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_keyboard_hook_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#start-mouse-capture")!
  .addEventListener("click", async () => {
    try {
      const gain = getFloatInput("#mouse-gain", 1.0);
      const intervalMs = getNumberInput("#capture-interval-ms", 33);
      const maxDelta = getNumberInput("#max-delta", 80);
      const remoteMode = document.querySelector<HTMLInputElement>("#remote-mode")?.checked ?? true;
      const switchEdge = document.querySelector<HTMLSelectElement>("#switch-edge")?.value ?? "right";
      const edgeMargin = getNumberInput("#edge-margin", 3);
      const remoteSize = getSelectedRemoteSize();

      await invoke<TcpSessionSnapshot>("start_mouse_capture", {
        gain,
        intervalMs,
        maxDelta,
        remoteMode,
        switchEdge,
        edgeMargin,
        remoteWidth: remoteSize.width,
        remoteHeight: remoteSize.height,
      });
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#stop-mouse-capture")!
  .addEventListener("click", async () => {
    try {
      await invoke<TcpSessionSnapshot>("stop_mouse_capture");
      await refreshTcpSession();
    } catch (error) {
      renderTcpError(error);
    }
  });

document
  .querySelector<HTMLButtonElement>("#apply-layout")!
  .addEventListener("click", () => {
    applyDisplayLayoutToControls();
  });

document
  .querySelector<HTMLButtonElement>("#reset-layout")!
  .addEventListener("click", () => {
    localStorage.removeItem(LAYOUT_STORAGE_KEY);
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLSelectElement>("#layout-peer")!
  .addEventListener("change", () => {
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-remote-width")!
  .addEventListener("change", () => {
    updateSavedRemoteSizeFromInputs();
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-remote-height")!
  .addEventListener("change", () => {
    updateSavedRemoteSizeFromInputs();
    renderDisplayLayoutEditor();
  });

document
  .querySelector<HTMLInputElement>("#layout-scale")!
  .addEventListener("change", () => {
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
refreshMonitorTopology().catch(renderMonitorError);
refreshTcpSession().catch(renderTcpError);

setInterval(() => {
  refreshTcpSession().catch(renderTcpError);
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
      </dl>
    </section>
  `;
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
    const onlineCount = status.peers.filter((peer) => peer.online).length;

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

async function refreshMonitorTopology() {
  const summary = document.querySelector<HTMLParagraphElement>("#monitor-summary")!;
  const list = document.querySelector<HTMLDivElement>("#monitor-list")!;

  summary.textContent = "Loading monitor topology...";
  list.innerHTML = `<div class="empty">Loading...</div>`;

  try {
    const topology = await invoke<MonitorTopology>("get_windows_monitor_topology");
    latestMonitorTopology = topology;
    renderDisplayLayoutEditor();
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
