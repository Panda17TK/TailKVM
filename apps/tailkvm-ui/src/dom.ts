// Small DOM / formatting helpers shared across every feature module.

import type { RectI32 } from "./types";

export const DEFAULT_PORT = 47110;

export function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

export function getFloatInput(selector: string, fallback: number): number {
  const input = document.querySelector<HTMLInputElement>(selector);
  const value = Number(input?.value.trim() ?? "");

  if (!Number.isFinite(value)) {
    return fallback;
  }

  return value;
}

export function getNumberInput(selector: string, fallback: number): number {
  const input = document.querySelector<HTMLInputElement>(selector)!;
  const value = Number(input.value.trim());

  if (!Number.isFinite(value)) {
    return fallback;
  }

  return Math.trunc(value);
}

export function setTextInputValue(selector: string, value: string) {
  const input = document.querySelector<HTMLInputElement>(selector);

  if (input) {
    input.value = value;
  }
}

export function getPortValue(): number {
  const input = document.querySelector<HTMLInputElement>("#tcp-port")!;
  const port = Number(input.value.trim() || DEFAULT_PORT);

  if (!Number.isFinite(port) || port < 1 || port > 65535) {
    return DEFAULT_PORT;
  }

  return Math.trunc(port);
}

export function formatRect(rect: RectI32): string {
  return `left=${rect.left}, top=${rect.top}, right=${rect.right}, bottom=${rect.bottom}, size=${rect.width}x${rect.height}`;
}
