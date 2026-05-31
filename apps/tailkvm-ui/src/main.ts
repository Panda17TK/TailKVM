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

const app = document.querySelector<HTMLDivElement>("#app")!;

app.innerHTML = `
  <main class="shell">
    <section class="hero">
      <div>
        <p class="eyebrow">Windows 11 + Tailscale Software KVM</p>
        <h1>TailKVM</h1>
        <p class="lead">
          Task 3: Read Windows monitor topology from Rust backend.
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
  .addEventListener("click", async () => {
    await refreshTailscaleStatus();
  });

document
  .querySelector<HTMLButtonElement>("#refresh-monitors")!
  .addEventListener("click", async () => {
    await refreshMonitorTopology();
  });

refreshTailscaleStatus().catch((error) => {
  renderTailscaleError(error);
});

refreshMonitorTopology().catch((error) => {
  renderMonitorError(error);
});

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
          <dd>${monitor.rect_physical_px.width} × ${monitor.rect_physical_px.height}px</dd>
        </div>
        <div>
          <dt>DPI</dt>
          <dd>${monitor.dpi_x} × ${monitor.dpi_y}</dd>
        </div>
      </dl>
    </section>
  `;
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
