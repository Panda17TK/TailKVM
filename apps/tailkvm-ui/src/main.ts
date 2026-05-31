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

          <button id="start-mouse-capture">Capture mouse</button>
          <button id="stop-mouse-capture">Stop capture</button>
        </div>

        <div id="tcp-state" class="tcp-state empty">Not loaded yet.</div>
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
  .querySelector<HTMLButtonElement>("#start-mouse-capture")!
  .addEventListener("click", async () => {
    try {
      const gain = getFloatInput("#mouse-gain", 1.0);
      const intervalMs = getNumberInput("#capture-interval-ms", 33);
      const maxDelta = getNumberInput("#max-delta", 80);

      await invoke<TcpSessionSnapshot>("start_mouse_capture", {
        gain,
        intervalMs,
        maxDelta,
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
