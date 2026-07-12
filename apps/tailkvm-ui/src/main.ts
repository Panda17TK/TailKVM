import "./styles.css";

import { withTimeout } from "./ipc";
import { mountApp } from "./template";
import { wireStatus, refreshTailscaleStatus, renderTailscaleError } from "./features/tailscale";
import {
  wireSessionStatus,
  refreshTcpSession,
  renderTcpError,
  refreshLockState,
} from "./features/session-status";
import { wireMonitors, refreshMonitorTopology, renderMonitorError } from "./features/monitors";
import { wireTcpConnection } from "./features/tcp-connection";
import { wireTcpInput } from "./features/tcp-input";
import { wireLayoutEditor } from "./features/layout-editor";
import { wireIme } from "./features/ime";
import { wireDisplayLayout } from "./features/display-layout";
import { wireQuickStart } from "./features/quickstart";

// Build the DOM, then wire every surface. Wiring only registers listeners and
// restores persisted state; no listener fires during setup, so the order between
// wire* calls is not behaviourally significant — the initial data loads below are.
mountApp();

wireStatus();
wireSessionStatus();
wireMonitors();
wireTcpConnection();
wireTcpInput();
wireLayoutEditor();
wireIme();
wireDisplayLayout();
wireQuickStart();

// Initial data load. refreshMonitorTopology retries the monitor command
// internally with a timeout, so a transient/early failure recovers on its own
// instead of leaving the panel stuck on "読込中...".
refreshTailscaleStatus().catch(renderTailscaleError);
refreshMonitorTopology().catch(renderMonitorError);
refreshTcpSession().catch(renderTcpError);
refreshLockState().catch(() => {});

// Self-rescheduling poll instead of a blind setInterval: the next tick is armed
// only after the previous one finishes, so a stalled backend can never stack
// overlapping in-flight calls; withTimeout bounds a hung call so the loop
// itself always advances.
const POLL_INTERVAL_MS = 2000;
const POLL_TIMEOUT_MS = 5000;

async function pollTick(): Promise<void> {
  try {
    await withTimeout(refreshTcpSession(), POLL_TIMEOUT_MS, "get_tcp_session_state");
  } catch (error) {
    renderTcpError(error);
  }
  try {
    await withTimeout(refreshLockState(), POLL_TIMEOUT_MS, "get_lock_state");
  } catch {
    // refreshLockState renders its own error into #lock-state.
  }
  window.setTimeout(() => void pollTick(), POLL_INTERVAL_MS);
}

window.setTimeout(() => void pollTick(), POLL_INTERVAL_MS);
