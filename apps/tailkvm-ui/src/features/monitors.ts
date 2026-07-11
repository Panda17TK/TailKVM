// Monitor Topology status card: loads the Windows virtual-screen / monitor layout
// (retried with a timeout so an early failure recovers on its own) and renders it.
// Also refreshes the two downstream views that depend on the topology.

import { invoke, withRetry, withTimeout } from "../ipc";
import { appState } from "../state";
import type { MonitorInfo, MonitorTopology } from "../types";
import { escapeHtml, formatRect } from "../dom";
import { renderDisplayLayoutEditor } from "./display-layout";
import { renderQuickStartMonitors } from "./quickstart";

export async function refreshMonitorTopology() {
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
    appState.latestMonitorTopology = topology;
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

export function renderMonitorError(error: unknown) {
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

/** Wire the "Refresh monitors" button. */
export function wireMonitors(): void {
  document
    .querySelector<HTMLButtonElement>("#refresh-monitors")
    ?.addEventListener("click", async () => refreshMonitorTopology());
}
