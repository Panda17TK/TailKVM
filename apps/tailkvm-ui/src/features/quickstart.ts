// Quick Start card: the primary flow (connect / receive / KVM control), the
// interactive monitor map with the draggable "相手PC" tile, per-host peer-screen
// and peer-attach persistence, KVM pointer-gain, and the status/advanced toggles.
// `renderQuickStartMonitors`, `updateQuickStartConn` and `savePeerScreen` are
// reused by the monitor and TCP-session refresh paths.

import { invoke } from "../ipc";
import { appState } from "../state";
import type { MonitorInfo, TcpSessionSnapshot } from "../types";
import { escapeHtml } from "../dom";
import { refreshTcpSession } from "./session-status";
import { getPrimaryTailscaleIp } from "./tailscale";

// True while seamless KVM capture is armed, so the flow stops pulsing "start".
let kvmActive = false;
// Previous connection state, for one-shot "connection succeeded" effects.
let wasConnected = false;

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
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return {};
    // Keep only entries that are a [w, h] pair of finite positive numbers, so
    // a corrupted store degrades to "unknown size" instead of NaN geometry.
    const valid: Record<string, [number, number]> = {};
    for (const [host, size] of Object.entries(parsed as Record<string, unknown>)) {
      if (
        Array.isArray(size) &&
        size.length === 2 &&
        size.every((n) => typeof n === "number" && Number.isFinite(n) && n > 0)
      ) {
        valid[host] = [size[0], size[1]];
      }
    }
    return valid;
  } catch {
    // ignore malformed storage
  }
  return {};
}

export function savePeerScreen(host: string, w: number, h: number) {
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

/** Find the monitor whose physical rect matches a stored attach rect. */
function findMonitorByRect(rect: [number, number, number, number]): MonitorInfo | undefined {
  return appState.latestMonitorTopology?.monitors.find(
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
export function renderQuickStartMonitors() {
  const box = document.querySelector<HTMLDivElement>("#qs-monitors");
  if (!box) return;
  const topo = appState.latestMonitorTopology;
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
  const cands = (appState.latestTailnetStatus?.peers ?? [])
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
    `<div id="qs-peer-tile" class="qs-peer-tile" tabindex="0" role="button" ` +
    `aria-label="相手PCの配置: 矢印キーで辺を選択、Enter または Space で確定" ` +
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
  // Apply a "drop" at virtual-desktop coordinates (vx, vy): pick the nearest
  // monitor + valid edge, place the peer rect and persist. Shared by the
  // pointer drop and the keyboard alternative so both take the exact same
  // attach code path.
  const applyDropAt = (vx: number, vy: number) => {
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
  };

  tile.addEventListener("pointerup", (ev) => {
    if (!dragging) return;
    dragging = false;
    tile.classList.remove("dragging");
    tile.releasePointerCapture(ev.pointerId);
    const rect = canvas.getBoundingClientRect();
    // Drop point in virtual-desktop coordinates.
    const vx = (ev.clientX - rect.left - pad) / scale + vs.left;
    const vy = (ev.clientY - rect.top - pad) / scale + vs.top;
    applyDropAt(vx, vy);
  });

  // Keyboard alternative to the drag (WCAG 2.1.1): arrow keys pin the peer to
  // that edge of the currently-attached (or primary) monitor by synthesizing a
  // drop just outside the edge, so snapping and multi-edge geometry stay
  // identical to the pointer path. Enter / Space re-applies the current edge.
  const applyEdgeKey = (e: KvmEdge) => {
    const OUT = 4; // just outside the edge, in virtual px
    const cx = (ar.left + ar.right) / 2;
    const cy = (ar.top + ar.bottom) / 2;
    const pt =
      e === "top"
        ? { x: cx, y: ar.top - OUT }
        : e === "bottom"
          ? { x: cx, y: ar.bottom + OUT }
          : e === "left"
            ? { x: ar.left - OUT, y: cy }
            : { x: ar.right + OUT, y: cy };
    applyDropAt(pt.x, pt.y);
    // The map re-rendered (innerHTML), so restore focus onto the new tile.
    document.querySelector<HTMLDivElement>("#qs-peer-tile")?.focus();
  };
  tile.addEventListener("keydown", (ev) => {
    const keyEdge: Partial<Record<string, KvmEdge>> = {
      ArrowUp: "top",
      ArrowDown: "bottom",
      ArrowLeft: "left",
      ArrowRight: "right",
    };
    const e = keyEdge[ev.key];
    if (e) {
      ev.preventDefault();
      applyEdgeKey(e);
      return;
    }
    if (ev.key === "Enter" || ev.key === " ") {
      ev.preventDefault();
      applyEdgeKey(edge);
    }
  });
}

export function updateQuickStartConn(snapshot: TcpSessionSnapshot) {
  const el = document.querySelector<HTMLSpanElement>("#qs-conn");
  if (!el) return;
  if (snapshot.connected) {
    const who = snapshot.peer_name || snapshot.peer_addr || "peer";
    el.textContent = `接続中: ${who}`;
    el.className = "qs-state qs-ok";
  } else if (snapshot.peer_addr) {
    // A connection was attempted but is not established — surface the reason
    // (connection refused = receiver not listening / firewall blocking).
    // Prefer the backend's structured classification; the regex remains only
    // as a fallback for older backends without last_event_is_error.
    const looksLikeError =
      snapshot.last_event_is_error ??
      /fail|refus|timed|error|closed|disconnect/i.test(snapshot.last_event);
    const reason = looksLikeError ? snapshot.last_event : "未接続";
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

/** Wire the Quick Start card: connect / receiver, KVM control, gain slider, and
 * the status/advanced toggles. */
export function wireQuickStart(): void {
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

  // KVM pointer-speed (gain) slider.
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
      btn.setAttribute("aria-expanded", on ? "true" : "false");
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
      btn.setAttribute("aria-expanded", on ? "true" : "false");
    }
  });
}
