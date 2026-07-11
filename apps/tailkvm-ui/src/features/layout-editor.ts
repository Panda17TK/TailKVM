// The two multi-screen layout editors inside the advanced TCP card: the visual
// left-to-right chain editor (le-*) and the 2D drag-placement editor (e2-* /
// editor-2d, links inferred from adjacency). Both build a screen/link config and
// drive connect_screen + router (start / reconfigure) + save_layout.

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { escapeHtml, getNumberInput, getPortValue } from "../dom";
import { refreshScreenList, refreshTcpSession, renderTcpError } from "./session-status";

function localScreenName(): string {
  return (
    document.querySelector<HTMLInputElement>("#router-local-name")?.value.trim() || "local"
  );
}

// --- Visual layout editor (left -> right chain) ---
type VisualScreen = { name: string; host: string };
let visualScreens: VisualScreen[] = [];

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

/** Wire both layout editors (visual chain + 2D placement) and render their
 * initial states. */
export function wireLayoutEditor(): void {
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
}
