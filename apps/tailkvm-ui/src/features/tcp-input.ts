// Advanced input-injection controls: mouse move/click/double-click tests, keyboard
// text and key-tap tests, the mouse and keyboard low-level capture start/stop,
// clipboard send + auto-sync, the JIS/US character-resolution toggle, and the Raw
// Input diagnostic.

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { getFloatInput, getNumberInput } from "../dom";
import { refreshTcpSession, renderTcpError } from "./session-status";
import { getSelectedRemoteSize } from "./display-layout";

async function sendTestMouseClick(button: "left" | "right" | "middle" | "x1" | "x2") {
  try {
    await invoke<TcpSessionSnapshot>("send_test_mouse_click", { button });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestKeyboardText(text: string) {
  try {
    await invoke<TcpSessionSnapshot>("send_test_keyboard_text", { text });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestKeyTap(key: string) {
  try {
    await invoke<TcpSessionSnapshot>("send_test_key_tap", { key });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

async function sendTestMouseDoubleClick(button: "left" | "right" | "middle" | "x1" | "x2") {
  try {
    await invoke<TcpSessionSnapshot>("send_test_mouse_double_click", { button });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

/** Wire the mouse/keyboard test, capture, clipboard and Raw Input controls. */
export function wireTcpInput(): void {
  document
    .querySelector<HTMLButtonElement>("#send-mouse-test")
    ?.addEventListener("click", async () => {
      const dx = getNumberInput("#mouse-dx", 80);
      const dy = getNumberInput("#mouse-dy", 0);

      await invoke<TcpSessionSnapshot>("send_test_mouse_move", { dx, dy });
      await refreshTcpSession();
    });

  document
    .querySelector<HTMLButtonElement>("#send-left-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseClick("left");
    });

  document
    .querySelector<HTMLButtonElement>("#send-right-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseClick("right");
    });

  document
    .querySelector<HTMLButtonElement>("#send-middle-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseClick("middle");
    });

  document
    .querySelector<HTMLButtonElement>("#send-x1-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseClick("x1");
    });

  document
    .querySelector<HTMLButtonElement>("#send-x2-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseClick("x2");
    });

  document
    .querySelector<HTMLButtonElement>("#send-left-double-click-test")
    ?.addEventListener("click", async () => {
      await sendTestMouseDoubleClick("left");
    });

  // NOTE: these two buttons (#start/stop-mouse-hook-capture) are not present in
  // the current DOM. Use optional chaining instead of `!` so a missing element
  // becomes a no-op rather than a TypeError that aborts the rest of this module's
  // top-level evaluation (which previously killed all initial data loading).
  document
    .querySelector<HTMLButtonElement>("#start-mouse-hook-capture")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("start_mouse_hook_capture");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#stop-mouse-hook-capture")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("stop_mouse_hook_capture");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#send-keyboard-text")
    ?.addEventListener("click", async () => {
      const text = document.querySelector<HTMLInputElement>("#keyboard-text")!.value;
      await sendTestKeyboardText(text);
    });

  document
    .querySelector<HTMLButtonElement>("#send-key-enter")
    ?.addEventListener("click", async () => {
      await sendTestKeyTap("enter");
    });

  document
    .querySelector<HTMLButtonElement>("#send-key-backspace")
    ?.addEventListener("click", async () => {
      await sendTestKeyTap("backspace");
    });

  document
    .querySelector<HTMLButtonElement>("#send-key-tab")
    ?.addEventListener("click", async () => {
      await sendTestKeyTap("tab");
    });

  document
    .querySelector<HTMLButtonElement>("#send-key-escape")
    ?.addEventListener("click", async () => {
      await sendTestKeyTap("escape");
    });

  document
    .querySelector<HTMLInputElement>("#clipboard-sync")
    ?.addEventListener("change", async (event) => {
      const enabled = (event.target as HTMLInputElement).checked;
      try {
        await invoke<TcpSessionSnapshot>("set_clipboard_sync", { enabled });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLInputElement>("#resolve-characters")
    ?.addEventListener("change", async (event) => {
      const enabled = (event.target as HTMLInputElement).checked;
      try {
        await invoke<TcpSessionSnapshot>("set_resolve_characters", { enabled });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#send-clipboard-text")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("send_clipboard_text");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#send-clipboard-image")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("send_clipboard_image");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#start-raw-mouse-diagnostic")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("start_raw_mouse_diagnostic");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#stop-raw-mouse-diagnostic")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("stop_raw_mouse_diagnostic");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#start-keyboard-hook-capture")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("start_keyboard_hook_capture");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#stop-keyboard-hook-capture")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("stop_keyboard_hook_capture");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#start-mouse-capture")
    ?.addEventListener("click", async () => {
      try {
        const gain = getFloatInput("#mouse-gain", 1.0);
        const intervalMs = getNumberInput("#capture-interval-ms", 8);
        const maxDelta = getNumberInput("#max-delta", 80);
        const remoteMode = document.querySelector<HTMLInputElement>("#remote-mode")?.checked ?? true;
        const switchEdge = document.querySelector<HTMLSelectElement>("#switch-edge")?.value ?? "right";
        const edgeMargin = getNumberInput("#edge-margin", 3);
        const remoteSize = getSelectedRemoteSize();
        const useRawInput =
          document.querySelector<HTMLInputElement>("#use-raw-input")?.checked ?? false;
        const seamless =
          document.querySelector<HTMLInputElement>("#seamless-mode")?.checked ?? false;
        const edgeDwellMs = getNumberInput("#edge-dwell-ms", 0);
        const deadCornerPx = getNumberInput("#dead-corner-px", 0);

        await invoke<TcpSessionSnapshot>("start_mouse_capture", {
          gain,
          intervalMs,
          maxDelta,
          remoteMode,
          switchEdge,
          edgeMargin,
          remoteWidth: remoteSize.width,
          remoteHeight: remoteSize.height,
          useRawInput,
          seamless,
          edgeDwellMs,
          deadCornerPx,
        });
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });

  document
    .querySelector<HTMLButtonElement>("#stop-mouse-capture")
    ?.addEventListener("click", async () => {
      try {
        await invoke<TcpSessionSnapshot>("stop_mouse_capture");
        await refreshTcpSession();
      } catch (error) {
        renderTcpError(error);
      }
    });
}
