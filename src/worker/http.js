/**
 * Shared HTTP entry point for upstream Rollercoin calls.
 *
 * Bun's global `fetch` already pools keep-alive connections per origin and
 * retires idle sockets internally, so the old undici Agent/dispatcher plumbing
 * is gone. Keeping one wrapper means the API client, warm probe and tests all
 * go through the same function.
 */

export const USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/** Bun-native fetch, kept as a named export for dependency injection. */
export function httpFetch(...args) {
  return globalThis.fetch(...args);
}
