import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SessionManager } from "../src/worker/session.js";
import { Store } from "../src/worker/store.js";

const silentLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {}
};

function tempStore() {
  const dir = mkdtempSync(join(tmpdir(), "turuc-session-"));
  const store = new Store({ dataDir: dir, logger: null }).load();
  store.setSession({
    accessToken: "access-token-123456",
    refreshToken: "refresh-token-123456",
    cookie: "cf=1"
  });
  return {
    store,
    cleanup() {
      rmSync(dir, { recursive: true, force: true });
    }
  };
}

test("clear removes every stored credential and emits the new status", () => {
  const { store, cleanup } = tempStore();
  try {
    const session = new SessionManager({ store, api: null, logger: silentLogger });
    let event = null;
    session.on("updated", (payload) => {
      event = payload;
    });

    const status = session.clear();
    assert.equal(status.hasAccessToken, false);
    assert.equal(status.hasRefreshToken, false);
    assert.equal(status.hasCookie, false);
    assert.equal(event.changed, true);
    assert.equal(event.status.hasAccessToken, false);
    assert.equal(store.state.session.accessToken, "");
    assert.equal(store.state.session.refreshToken, "");
    assert.equal(store.state.session.cookie, "");
  } finally {
    cleanup();
  }
});

test("clearing during an in-flight refresh discards the late result", async () => {
  const { store, cleanup } = tempStore();
  try {
    let resolveRefresh;
    const api = {
      refreshSession() {
        return new Promise((resolve) => {
          resolveRefresh = resolve;
        });
      }
    };
    const session = new SessionManager({ store, api, logger: silentLogger });

    const pending = session.refresh("manual", { force: true });
    assert.equal(typeof resolveRefresh, "function");

    session.clear();
    resolveRefresh({
      ok: true,
      accessToken: "late-access-token",
      refreshToken: "late-refresh-token"
    });

    assert.equal(await pending, false);
    assert.equal(store.state.session.accessToken, "");
    assert.equal(store.state.session.refreshToken, "");
  } finally {
    cleanup();
  }
});
