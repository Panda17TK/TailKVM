// Japanese IME settings (IME-UI-002 / IME-CONF-001..003): persist the candidate
// position / open / conversion / focus policies to localStorage, mirror them into
// the UI, push them to the backend, and support one-click presets.

import { invoke } from "../ipc";
import type { TcpSessionSnapshot } from "../types";
import { refreshTcpSession, renderTcpError } from "./session-status";

type ImeSettings = {
  version: number;
  candidatePositionMode: string;
  imeOpenPolicy: string;
  conversionModePolicy: string;
  focusFailurePolicy: string;
  fixedX: number;
  fixedY: number;
  captureWindowSize: number;
  lockNearOffset: number;
};

const IME_SETTINGS_KEY = "tailkvm.imeSettings.v1";

const DEFAULT_IME_SETTINGS: ImeSettings = {
  version: 1,
  candidatePositionMode: "remote_projected",
  imeOpenPolicy: "force_japanese",
  conversionModePolicy: "native_default",
  focusFailurePolicy: "retry",
  fixedX: 0,
  fixedY: 0,
  captureWindowSize: 1,
  lockNearOffset: 24,
};

// IME state presets (P2): one-click policy combinations. Fields not listed
// keep their current values.
const IME_PRESETS: Record<string, Partial<ImeSettings>> = {
  standard_japanese: {
    candidatePositionMode: "remote_projected",
    imeOpenPolicy: "force_japanese",
    conversionModePolicy: "native_default",
    focusFailurePolicy: "retry",
  },
  preserve_current: {
    imeOpenPolicy: "preserve_current",
    conversionModePolicy: "preserve",
    focusFailurePolicy: "warn_continue",
  },
  last_session: {
    imeOpenPolicy: "restore_last_tailkvm",
    conversionModePolicy: "last_used",
    focusFailurePolicy: "retry",
  },
};

function loadImeSettings(): ImeSettings {
  try {
    const raw = localStorage.getItem(IME_SETTINGS_KEY);
    if (!raw) return { ...DEFAULT_IME_SETTINGS };
    const parsed = JSON.parse(raw) as Partial<ImeSettings>;
    const merged = { ...DEFAULT_IME_SETTINGS, ...parsed, version: 1 };
    // Spread-merge covers missing fields but not wrong-typed ones: coerce any
    // corrupted field back to its default so e.g. a string fixedX can't reach
    // the backend as NaN.
    for (const key of ["fixedX", "fixedY", "captureWindowSize", "lockNearOffset"] as const) {
      if (typeof merged[key] !== "number" || !Number.isFinite(merged[key])) {
        merged[key] = DEFAULT_IME_SETTINGS[key];
      }
    }
    for (const key of [
      "candidatePositionMode",
      "imeOpenPolicy",
      "conversionModePolicy",
      "focusFailurePolicy",
    ] as const) {
      if (typeof merged[key] !== "string") {
        merged[key] = DEFAULT_IME_SETTINGS[key];
      }
    }
    return merged;
  } catch {
    return { ...DEFAULT_IME_SETTINGS };
  }
}

function readImeSettingsFromUi(): ImeSettings {
  const select = (id: string, fallback: string): string =>
    document.querySelector<HTMLSelectElement>(id)?.value ?? fallback;
  const number = (id: string): number =>
    Number(document.querySelector<HTMLInputElement>(id)?.value) || 0;
  return {
    version: 1,
    candidatePositionMode: select(
      "#ime-candidate-position",
      DEFAULT_IME_SETTINGS.candidatePositionMode,
    ),
    imeOpenPolicy: select("#ime-open-policy", DEFAULT_IME_SETTINGS.imeOpenPolicy),
    conversionModePolicy: select(
      "#ime-conversion-policy",
      DEFAULT_IME_SETTINGS.conversionModePolicy,
    ),
    focusFailurePolicy: select("#ime-focus-policy", DEFAULT_IME_SETTINGS.focusFailurePolicy),
    fixedX: number("#ime-fixed-x"),
    fixedY: number("#ime-fixed-y"),
    captureWindowSize:
      number("#ime-window-size") || DEFAULT_IME_SETTINGS.captureWindowSize,
    lockNearOffset: number("#ime-lock-offset"),
  };
}

function applyImeSettingsToUi(settings: ImeSettings): void {
  const set = (id: string, value: string): void => {
    const element = document.querySelector<HTMLSelectElement | HTMLInputElement>(id);
    if (element) element.value = value;
  };
  set("#ime-candidate-position", settings.candidatePositionMode);
  set("#ime-open-policy", settings.imeOpenPolicy);
  set("#ime-conversion-policy", settings.conversionModePolicy);
  set("#ime-focus-policy", settings.focusFailurePolicy);
  set("#ime-fixed-x", String(settings.fixedX));
  set("#ime-fixed-y", String(settings.fixedY));
  set("#ime-window-size", String(settings.captureWindowSize));
  set("#ime-lock-offset", String(settings.lockNearOffset));
}

async function pushImeSettings(settings: ImeSettings): Promise<void> {
  localStorage.setItem(IME_SETTINGS_KEY, JSON.stringify(settings));
  try {
    await invoke<TcpSessionSnapshot>("set_ime_settings", {
      settings: {
        candidatePositionMode: settings.candidatePositionMode,
        imeOpenPolicy: settings.imeOpenPolicy,
        conversionModePolicy: settings.conversionModePolicy,
        focusFailurePolicy: settings.focusFailurePolicy,
        fixedX: settings.fixedX,
        fixedY: settings.fixedY,
        captureWindowSize: settings.captureWindowSize,
        lockNearOffset: settings.lockNearOffset,
      },
    });
    await refreshTcpSession();
  } catch (error) {
    renderTcpError(error);
  }
}

/** Restore persisted IME settings into the UI, push them to the backend, and wire
 * the per-field change handlers plus the preset selector. */
export function wireIme(): void {
  applyImeSettingsToUi(loadImeSettings());
  // Push the persisted settings to the backend on startup so composition
  // mode uses them even before the user touches the controls.
  void pushImeSettings(readImeSettingsFromUi());
  for (const id of [
    "#ime-candidate-position",
    "#ime-open-policy",
    "#ime-conversion-policy",
    "#ime-focus-policy",
    "#ime-fixed-x",
    "#ime-fixed-y",
    "#ime-window-size",
    "#ime-lock-offset",
  ]) {
    document.querySelector<HTMLElement>(id)?.addEventListener("change", () => {
      void pushImeSettings(readImeSettingsFromUi());
    });
  }
  // Preset selector: applies a policy combination on top of the current
  // values, then resets itself so it reads as an action, not a state.
  document.querySelector<HTMLSelectElement>("#ime-preset")?.addEventListener("change", (event) => {
    const select = event.target as HTMLSelectElement;
    const preset = IME_PRESETS[select.value];
    if (preset) {
      const merged = { ...readImeSettingsFromUi(), ...preset };
      applyImeSettingsToUi(merged);
      void pushImeSettings(merged);
    }
    select.value = "";
  });
}
