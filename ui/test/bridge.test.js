import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import vm from "node:vm";

test("userscript bridge uses GM_xmlhttpRequest and writes synced tokens back", async () => {
  const source = readFileSync(resolve(import.meta.dirname, "../../public/bridge.js"), "utf8")
    .replace("__WORKER_URL__", "http://127.0.0.1:8787")
    .replace("__WORKER_TOKEN__", "worker-access-key");

  const storage = {
    data: { token: "page-access-token", refreshToken: "page-refresh-token" },
    getItem(key) {
      return this.data[key] ?? null;
    },
    setItem(key, value) {
      this.data[key] = String(value);
    }
  };
  const page = { localStorage: storage };
  const logs = [];
  let request = null;

  const sandbox = {
    window: page,
    unsafeWindow: page,
    localStorage: storage,
    console: {
      log: (...args) => logs.push(args.join(" ")),
      warn: (...args) => logs.push(args.join(" "))
    },
    GM_xmlhttpRequest(options) {
      request = options;
      setTimeout(() => {
        options.onload({
          status: 200,
          responseText: JSON.stringify({
            ok: true,
            accepted: true,
            session: {
              accessToken: "worker-new-access",
              refreshToken: "worker-new-refresh"
            }
          })
        });
      }, 0);
    },
    setInterval() {
      return 1;
    },
    clearInterval() {},
    setTimeout,
    clearTimeout,
    JSON,
    URL
  };
  sandbox.globalThis = sandbox;

  vm.runInNewContext(source, sandbox, { filename: "bridge.js" });
  await new Promise((resolveWait) => setTimeout(resolveWait, 30));

  assert.ok(request, "GM request should have been made");
  assert.equal(request.method, "POST");
  assert.equal(request.url, "http://127.0.0.1:8787/api/session/push");
  assert.equal(request.headers.authorization, "Bearer worker-access-key");
  assert.equal(JSON.parse(request.data).accessToken, "page-access-token");
  assert.equal(storage.data.token, "worker-new-access");
  assert.equal(storage.data.refreshToken, "worker-new-refresh");
  assert.ok(logs.some((line) => line.includes("GM_xmlhttpRequest")));

  page.__TURU_RC_BRIDGE__.stop();
});
