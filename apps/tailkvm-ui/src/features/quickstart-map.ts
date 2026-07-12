// The Quick Start interactive monitor map: this PC's real monitors plus the
// draggable (and keyboard-operable) "相手PC" tile, the connection-candidate
// list, and the drop→attach geometry (nearest monitor, outer-edge snap,
// corner multi-edge placement). Persistence lives in quickstart-storage; the
// flow wiring lives in quickstart.

import { appState } from "../state";
import type { MonitorInfo } from "../types";
import { escapeHtml } from "../dom";
import { getPrimaryTailscaleIp } from "./tailscale";
import {
  EDGE_LABEL,
  getPeerAttach,
  getPeerScreenForHost,
  savePeerAttach,
  type KvmEdge,
} from "./quickstart-storage";

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
