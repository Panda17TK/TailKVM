// Thin wrapper around the Tauri `invoke` bridge plus the async helpers used to
// make backend calls resilient (timeout + retry). Every backend command name and
// argument shape is a contract with the Rust side and is passed through verbatim.

import { invoke } from "@tauri-apps/api/core";

export { invoke };

export const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** Reject if a promise does not settle within `ms`, so a hung invoke can't
 * leave a panel spinning forever (and lets withRetry actually retry it). */
export function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((_, reject) =>
      setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms),
    ),
  ]);
}

/** Retry an async operation a few times with a short delay between attempts. */
export async function withRetry<T>(fn: () => Promise<T>, attempts = 6, delayMs = 350): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await fn();
    } catch (error) {
      lastError = error;
      if (attempt < attempts - 1) {
        await sleep(delayMs);
      }
    }
  }
  throw lastError;
}
