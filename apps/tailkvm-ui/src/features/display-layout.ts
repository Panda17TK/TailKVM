// Display Layout Editor: arrange the remote display like Windows display settings
// (drag the remote tile, infer the switch edge). Persists to localStorage and can
// push the chosen peer IP / switch edge into the advanced controls. Owns the
// document-level pointer drag for the remote tile and `getSelectedRemoteSize`,
// which the router and mouse-capture controls reuse.

import { appState } from "../state";
import type { LayoutRect, SavedDisplayLayout } from "../types";
import { escapeHtml, getFloatInput, getNumberInput, setTextInputValue } from "../dom";
import { getPrimaryTailscaleIp } from "./tailscale";
import { renderTcpInfo } from "./session-status";

const LAYOUT_STORAGE_KEY = "tailkvm.displayLayout.v1";

// Keyboard nudge for the remote box: canvas pixels per arrow press (converted
// through the canvas scale like the pointer drag), Shift for coarse moves.
const LAYOUT_KEY_STEP_PX = 24;
const LAYOUT_KEY_SHIFT_MULTIPLIER = 4;
const LAYOUT_KEY_DIRECTIONS: Partial<Record<string, [number, number]>> = {
  ArrowLeft: [-1, 0],
  ArrowRight: [1, 0],
  ArrowUp: [0, -1],
  ArrowDown: [0, 1],
};

type LayoutDragState = {
  startClientX: number;
  startClientY: number;
  startRect: LayoutRect;
};

let layoutDragState: LayoutDragState | null = null;

export function populateLayoutPeerSelect() {
  const select = document.querySelector<HTMLSelectElement>("#layout-peer");

  if (!select || !appState.latestTailnetStatus) {
    return;
  }

  const saved = loadDisplayLayout();
  const current = select.value || saved?.targetPeerIp || "";

  const peers = appState.latestTailnetStatus.peers
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

export function renderDisplayLayoutEditor() {
  const summary = document.querySelector<HTMLParagraphElement>("#layout-summary");
  const canvas = document.querySelector<HTMLDivElement>("#layout-canvas");

  if (!summary || !canvas) {
    return;
  }

  populateLayoutPeerSelect();

  if (!appState.latestMonitorTopology) {
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

  const localVirtual = appState.latestMonitorTopology.virtual_screen;
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

  const monitorHtml = appState.latestMonitorTopology.monitors
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
    <div class="layout-monitor remote layout-remote" tabindex="0" role="button"
      aria-label="リモート画面の位置: 矢印キーで移動（Shift で大きく移動）" style="${remoteStyle}">
      <div class="layout-monitor-title">${escapeHtml(layout.targetPeerHost || "Remote peer")}</div>
      <div class="layout-monitor-subtitle">${Math.round(layout.remoteRect.width)} x ${Math.round(layout.remoteRect.height)}</div>
      <div class="layout-monitor-badge">REMOTE</div>
      <div class="layout-drag-hint">drag</div>
    </div>
  `;

  summary.textContent =
    `Target: ${layout.targetPeerHost || layout.targetPeerIp} / IP: ${layout.targetPeerIp} / inferred switch edge: ${layout.switchEdge}`;
}

export function getCurrentDisplayLayout(): SavedDisplayLayout | null {
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

  if (!appState.latestMonitorTopology) {
    return null;
  }

  const virtual = appState.latestMonitorTopology.virtual_screen;
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

function inferSwitchEdge(remoteRect: LayoutRect): "left" | "right" | "top" | "bottom" {
  if (!appState.latestMonitorTopology) {
    return "right";
  }

  const local = appState.latestMonitorTopology.virtual_screen;
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

export function getSelectedRemoteSize(): { width: number; height: number } {
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

/** Wire the Display Layout Editor controls and the document-level drag of the
 * remote tile. */
export function wireDisplayLayout(): void {
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

  // Keyboard alternative to the pointer drag (WCAG 2.1.1): arrow keys nudge the
  // focused remote box by a canvas grid step (Shift = larger), through the same
  // state-update + persist path the pointer drag uses.
  document.addEventListener("keydown", (event) => {
    const target = event.target;

    if (!(target instanceof HTMLElement) || !target.closest(".layout-remote")) {
      return;
    }

    const dir = LAYOUT_KEY_DIRECTIONS[event.key];

    if (!dir) {
      return;
    }

    event.preventDefault();

    const layout = getCurrentDisplayLayout();

    if (!layout) {
      return;
    }

    const scale = getLayoutScale();
    const step =
      (event.shiftKey ? LAYOUT_KEY_STEP_PX * LAYOUT_KEY_SHIFT_MULTIPLIER : LAYOUT_KEY_STEP_PX) /
      scale;

    layout.remoteRect = {
      ...layout.remoteRect,
      x: Math.round(layout.remoteRect.x + dir[0] * step),
      y: Math.round(layout.remoteRect.y + dir[1] * step),
    };

    layout.switchEdge = inferSwitchEdge(layout.remoteRect);
    saveDisplayLayout(layout);
    renderDisplayLayoutEditor();
    // The canvas re-rendered (innerHTML), so restore focus onto the new box.
    document.querySelector<HTMLElement>(".layout-remote")?.focus();
  });
}
