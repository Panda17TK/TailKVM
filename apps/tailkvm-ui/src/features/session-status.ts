// TCP session status: polling the backend snapshot, rendering it (plus the mirror
// into the IME status banner), and the shared error/info renderers reused by the
// whole advanced TCP surface. Also owns the screen-list and lock-state panels.

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { escapeHtml } from "../dom";
import { savePeerScreen, updateQuickStartConn } from "./quickstart";

export async function refreshScreenList() {
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

export async function refreshLockState() {
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

export async function refreshTcpSession() {
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

export function renderTcpSession(state: TcpSessionSnapshot) {
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

  // Safety-critical capture indicator (always-visible HUD): derived ONLY from
  // the backend snapshot, never from a frontend guess. 操作中 = this machine's
  // input is being forwarded to a peer; 被操作中 = a peer session is accepted
  // and may inject here; 待機 = neither.
  const hudCapture = document.querySelector<HTMLElement>("#hud-capture");
  if (hudCapture) {
    const capturing = state.capture_active === true;
    const controlled = state.role === "receiver" && state.connected;
    hudCapture.innerHTML = capturing
      ? `<i class="hud-lamp warn"></i>操作中`
      : controlled
        ? `<i class="hud-lamp hot"></i>被操作中`
        : `<i class="hud-lamp"></i>待機`;
  }

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

// Action feedback targets #action-alert / #action-feedback — dedicated nodes
// OUTSIDE everything the 2s poll re-renders, so a user-action message can no
// longer be clobbered within 2 seconds by renderTcpSession. Info and errors use
// separate elements because their ARIA roles differ (status vs alert) and a
// live region only announces when its own content changes.

export function renderTcpError(error: unknown) {
  const alertBox = document.querySelector<HTMLDivElement>("#action-alert");
  const infoBox = document.querySelector<HTMLDivElement>("#action-feedback");
  if (alertBox) alertBox.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
  if (infoBox) infoBox.innerHTML = "";
}

export function renderTcpInfo(message: string) {
  const alertBox = document.querySelector<HTMLDivElement>("#action-alert");
  const infoBox = document.querySelector<HTMLDivElement>("#action-feedback");
  if (infoBox) infoBox.innerHTML = `<div class="info-box">${escapeHtml(message)}</div>`;
  if (alertBox) alertBox.innerHTML = "";
}

/** Wire the "Refresh TCP state" button in the advanced card. */
export function wireSessionStatus(): void {
  document
    .querySelector<HTMLButtonElement>("#refresh-tcp")
    ?.addEventListener("click", async () => refreshTcpSession());
}
