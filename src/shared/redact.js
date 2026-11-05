const SECRET_QUERY_RE = /([?&](?:token|access_token|refresh_token|session|key)=)[^&\s'"]+/gi;
const BEARER_RE = /\b(Bearer\s+)[A-Za-z0-9._~+/-]+=*/gi;

/**
 * Removes credentials from a URL/error string before it is logged, stored in
 * `lastError`, or broadcast to the control deck.
 *
 * The upstream market feed receives its JWT as `?token=...`, so a failed
 * WebSocket handshake otherwise prints the full session token in the worker
 * terminal and in `/api/state`.
 */
export function redactSecrets(value) {
  return String(value ?? "")
    .replace(SECRET_QUERY_RE, "$1<redacted>")
    .replace(BEARER_RE, "$1<redacted>");
}
