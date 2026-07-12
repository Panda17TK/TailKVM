// Quick Start card — the primary flow wiring: connect / receiver, KVM
// start/stop/emergency, the gain slider, connection-state guidance, and the
// status/advanced toggles. The interactive monitor map lives in quickstart-map;
// persistence (peer attach / peer screens / gain) lives in quickstart-storage.
// The re-exports keep `./quickstart` the stable import path for consumers
// (main.ts, monitors.ts, session-status.ts).

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { refreshTcpSession } from "./session-status";
import {
  EDGE_LABEL,
  getKvmEdge,
  getKvmGain,
  getPeerAttach,
  KVM_GAIN_KEY,
} from "./quickstart-storage";

export { renderQuickStartMonitors } from "./quickstart-map";
export { savePeerScreen } from "./quickstart-storage";

// True while seamless KVM capture is armed, so the flow stops pulsing "start".
let kvmActive = false;
// Previous connection state, for one-shot "connection succeeded" effects.
let wasConnected = false;

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
