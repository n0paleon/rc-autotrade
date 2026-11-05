import { test } from "bun:test";
import assert from "node:assert/strict";
import { redactSecrets } from "../src/shared/redact.js";

test("redacts market-feed tokens from WebSocket errors", () => {
  const message =
    "WebSocket connection to 'wss://nws.rollercoin.com/?token=eyJhbGciOiJIUzI1NiJ9.payload.signature' failed: TLS handshake failed";
  const redacted = redactSecrets(message);
  assert.equal(
    redacted,
    "WebSocket connection to 'wss://nws.rollercoin.com/?token=<redacted>' failed: TLS handshake failed"
  );
  assert.ok(!redacted.includes("eyJhbGciOiJIUzI1NiJ9"));
});

test("redacts bearer headers and keeps unrelated errors intact", () => {
  assert.equal(redactSecrets("Authorization: Bearer abc.def.ghi"), "Authorization: Bearer <redacted>");
  assert.equal(redactSecrets("socket closed (1006)"), "socket closed (1006)");
  assert.equal(redactSecrets(undefined), "");
});
