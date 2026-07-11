// Tailscale status surface: the Runtime, Tailscale, Keyboard Layout status cards,
// the "This machine" / "Peers" node lists, and the HUD mirror. Also owns the
// peer-action buttons (Use for Connect / Use for Firewall) delegated at document
// level, and the shared `getPrimaryTailscaleIp` helper.

import { invoke } from "../ipc";
import { appState } from "../state";
import type { KeyboardLayoutInfo, TailnetNode, TailnetStatus } from "../types";
import { escapeHtml, setTextInputValue } from "../dom";
import { populateLayoutPeerSelect, renderDisplayLayoutEditor } from "./display-layout";
import { renderTcpInfo } from "./session-status";

export function getPrimaryTailscaleIp(node: TailnetNode): string {
  return node.tailscale_ips.find((value) => value.includes(".")) ?? node.tailscale_ips[0] ?? "";
}

export async function refreshTailscaleStatus() {
  const summary = document.querySelector<HTMLParagraphElement>("#tailscale-summary")!;
  const selfNode = document.querySelector<HTMLDivElement>("#self-node")!;
  const peerList = document.querySelector<HTMLDivElement>("#peer-list")!;

  summary.textContent = "Loading tailscale status...";
  selfNode.innerHTML = `<div class="empty">Loading...</div>`;
  peerList.innerHTML = `<div class="empty">Loading...</div>`;

  try {
    const status = await invoke<TailnetStatus>("get_tailscale_status");
    appState.latestTailnetStatus = status;
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

export async function refreshKeyboardLayout() {
  const summary = document.querySelector<HTMLParagraphElement>("#keyboard-layout-summary")!;
  summary.textContent = "Loading keyboard layout...";

  try {
    const info = await invoke<KeyboardLayoutInfo>("get_keyboard_layout");
    summary.textContent = info.label;
  } catch (error) {
    summary.textContent = `Keyboard layout error: ${String(error)}`;
  }
}

export function renderTailscaleError(error: unknown) {
  const summary = document.querySelector<HTMLParagraphElement>("#tailscale-summary")!;
  const selfNode = document.querySelector<HTMLDivElement>("#self-node")!;
  const peerList = document.querySelector<HTMLDivElement>("#peer-list")!;

  summary.textContent = "Failed to load tailscale status.";
  selfNode.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
  peerList.innerHTML = `<div class="empty">Fix the error above, then refresh.</div>`;
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

/** Wire the Runtime / Tailscale / Keyboard status cards plus the peer-action
 * delegated click handler that fills the Connect/Firewall fields. */
export function wireStatus(): void {
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
    .querySelector<HTMLButtonElement>("#refresh-keyboard-layout")
    ?.addEventListener("click", async () => refreshKeyboardLayout());

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
}
