import { Agent } from "undici";

export { fetch as httpFetch } from "undici";

export const USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const DEFAULT_KEEP_ALIVE_MS = 60_000;
const DEFAULT_CONNECTIONS = 8;
const DEFAULT_CONNECT_TIMEOUT_MS = 10_000;

/**
 * Shared keep-alive pool for every Rollercoin HTTP call.
 *
 * Node's global `fetch` closes idle sockets after ~4s, which turns any request
 * that follows a quiet period into a fresh TCP+TLS handshake. A long-lived
 * Agent keeps the edge connection reusable between purchases.
 */
export function createHttpDispatcher({
  keepAliveMs = DEFAULT_KEEP_ALIVE_MS,
  connections = DEFAULT_CONNECTIONS,
  connectTimeoutMs = DEFAULT_CONNECT_TIMEOUT_MS
} = {}) {
  return new Agent({
    keepAliveTimeout: Math.max(1_000, keepAliveMs),
    keepAliveMaxTimeout: Math.max(120_000, keepAliveMs),
    keepAliveTimeoutThreshold: 1_000,
    connections: Math.max(1, connections),
    connect: { timeout: Math.max(1_000, connectTimeoutMs) }
  });
}
