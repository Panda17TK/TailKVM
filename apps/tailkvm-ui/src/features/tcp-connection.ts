// Advanced TCP connection controls: pairing token (H1), receiver / connect /
// disconnect / accept-incoming, peer discovery, single-screen connect/disconnect,
// the right-chain router, the raw layout-JSON load/save, and firewall install.

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { escapeHtml, getNumberInput, getPortValue } from "../dom";
import {
  refreshScreenList,
  refreshTcpSession,
  renderTcpError,
  renderTcpInfo,
} from "./session-status";
import { getSelectedRemoteSize } from "./display-layout";

// H1 pairing token: persisted in localStorage and pushed to the backend, which
// requires it on inbound Hello and sends it on outbound Hello. Empty = off.
const AUTH_TOKEN_STORAGE_KEY = "tailkvm.authToken.v1";

async function pushAuthToken(): Promise<void> {
  const token = document.querySelector<HTMLInputElement>("#auth-token")?.value ?? "";
  try {
    localStorage.setItem(AUTH_TOKEN_STORAGE_KEY, token);
  } catch {
    // Ignore storage failures (e.g. private mode); the token still applies live.
  }
  await invoke<TcpSessionSnapshot>("set_auth_token", { token });
}

function restoreAuthTokenInput(): void {
  const input = document.querySelector<HTMLInputElement>("#auth-token");
  if (!input) return;
  try {
    input.value = localStorage.getItem(AUTH_TOKEN_STORAGE_KEY) ?? "";
  } catch {
    // Ignore storage failures.
  }
}

/** Wire the advanced connection/router/layout-JSON/firewall controls, restore the
 * saved pairing token, and push it so a controller that connects before starting
 * the receiver still presents it. */
export function wireTcpConnection(): void {
  document
    .querySelector<HTMLButtonElement>("#install-firewall")
    ?.addEventListener("click", async () => {
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
    .querySelector<HTMLButtonElement>("#start-receiver")
    ?.addEventListener("click", async () => {
      const port = getPortValue();
      const tailnetOnly =
        document.querySelector<HTMLInputElement>("#tailnet-only")?.checked ?? false;
      // Push the current pairing token first so the receiver enforces it from the
      // first handshake (H1).
      await pushAuthToken();
      await invoke<TcpSessionSnapshot>("start_tcp_receiver", {
        port,
        tailnetOnly,
      });
      await refreshTcpSession();
    });

  document
    .querySelector<HTMLButtonElement>("#connect-peer")
    ?.addEventListener("click", async () => {
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
    .querySelector<HTMLButtonElement>("#disconnect-peer")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("disconnect_tcp_peer");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLInputElement>("#accept-incoming")
    ?.addEventListener("change", async (event) => {
      const enabled = (event.target as HTMLInputElement).checked;
      try {
        await invoke<TcpSessionSnapshot>("set_accept_incoming", { enabled });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLInputElement>("#auth-token")
    ?.addEventListener("change", async () => {
      try {
        await pushAuthToken();
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#connect-screen")
    ?.addEventListener("click", async () => {
      const name = document.querySelector<HTMLInputElement>("#screen-name")!.value.trim();
      const host = document.querySelector<HTMLInputElement>("#screen-host")!.value.trim();
      const port = getPortValue();
      if (!name || !host) {
        renderTcpError("Screen name and host are required.");
        return;
      }
      try {
        await invoke<TcpSessionSnapshot>("connect_screen", { name, host, port });
        await refreshScreenList();
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#disconnect-screen")
    ?.addEventListener("click", async () => {
      const name = document.querySelector<HTMLInputElement>("#screen-name")!.value.trim();
      if (!name) {
        renderTcpError("Screen name is required.");
        return;
      }
      try {
        await invoke<TcpSessionSnapshot>("disconnect_screen", { name });
        await refreshScreenList();
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#list-screens")
    ?.addEventListener("click", async () => {
      await refreshScreenList();
    });

  document
    .querySelector<HTMLButtonElement>("#start-router")
    ?.addEventListener("click", async () => {
      try {
        const localName =
          document.querySelector<HTMLInputElement>("#router-local-name")!.value.trim() || "local";
        const screens = await invoke<{ name: string; connected: boolean }[]>("list_screens");
        const remoteSize = getSelectedRemoteSize();

        // Build a simple left-to-right chain: local -> screen1 -> screen2 -> ...
        const configScreens = [
          { name: localName, width: 0, height: 0, is_local: true },
          ...screens.map((s) => ({
            name: s.name,
            width: remoteSize.width,
            height: remoteSize.height,
            is_local: false,
          })),
        ];
        const chain = [localName, ...screens.map((s) => s.name)];
        const links = chain.slice(0, -1).map((from, i) => ({
          from,
          edge: "right",
          to: chain[i + 1],
        }));

        if (links.length === 0) {
          renderTcpError("Connect at least one screen before starting the router.");
          return;
        }

        const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
        const deadCornerPx = getNumberInput("#dead-corner-px", 0);
        await invoke<TcpSessionSnapshot>("start_multi_screen_router", {
          config: { screens: configScreens, links },
          edgeDwellMs,
          deadCornerPx,
        });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#stop-router")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("stop_multi_screen_router");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#load-layout")
    ?.addEventListener("click", async () => {
      try {
        const layout = await invoke<unknown>("load_layout");
        document.querySelector<HTMLTextAreaElement>("#layout-json")!.value = JSON.stringify(
          layout,
          null,
          2,
        );
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#save-layout")
    ?.addEventListener("click", async () => {
      const raw = document.querySelector<HTMLTextAreaElement>("#layout-json")!.value.trim();
      let layout: unknown;
      try {
        layout = JSON.parse(raw);
      } catch {
        renderTcpError("Layout JSON is invalid.");
        return;
      }
      try {
        await invoke<TcpSessionSnapshot>("save_layout", { layout });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#discover-peers")
    ?.addEventListener("click", async () => {
      const box = document.querySelector<HTMLDivElement>("#discovered-peers")!;
      box.textContent = "Discovering...";
      try {
        const port = getPortValue();
        const peers = await invoke<
          { host_name: string; ip: string; reachable: boolean }[]
        >("discover_tailkvm_peers", { port });
        if (peers.length === 0) {
          box.textContent = "No online peers found.";
          return;
        }
        box.innerHTML = peers
          .map(
            (p) =>
              `<div>${p.reachable ? "✅" : "—"} ${escapeHtml(p.host_name)} (${escapeHtml(p.ip)})${p.reachable ? " — TailKVM port open" : ""}</div>`,
          )
          .join("");
      } catch (error) {
        box.innerHTML = `<div class="error-box">${escapeHtml(String(error))}</div>`;
      }
    });

  // Restore the saved token on load and push it so a controller that connects
  // before starting the receiver still presents it.
  restoreAuthTokenInput();
  void invoke<TcpSessionSnapshot>("set_auth_token", {
    token: document.querySelector<HTMLInputElement>("#auth-token")?.value ?? "",
  }).catch(() => {});
}
