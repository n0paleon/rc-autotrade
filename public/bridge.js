/* ════════════════════════════════════════════════════════════════════
   TURU.RC // TOKEN BRIDGE
   Runs in one of two ways:
     1. Pasted into the DevTools console on rollercoin.com (page fetch).
     2. Installed as a Tampermonkey userscript (GM_xmlhttpRequest).

   The userscript transport bypasses the page's CORS/CSP restrictions, which
   is required on sites that block requests to localhost. The bridge pushes
   the localStorage session to the worker every 30 seconds and writes renewed
   tokens back into the page.

   Replacements are applied by the control deck before you copy it.
   ════════════════════════════════════════════════════════════════════ */
(() => {
  "use strict";

  const WORKER_URL = "__WORKER_URL__";
  const WORKER_TOKEN = "__WORKER_TOKEN__";
  const INTERVAL_MS = 30000;

  // Tampermonkey sandbox exposes the real page through unsafeWindow. In console
  // mode this is simply window. Storage must be the page's localStorage, not a
  // sandbox-local copy, so the site's own token refresh stays in sync.
  const pageWindow =
    typeof unsafeWindow !== "undefined" && unsafeWindow ? unsafeWindow : window;
  const storage = (() => {
    try {
      return pageWindow.localStorage;
    } catch {
      return window.localStorage;
    }
  })();

  const gmRequest = typeof GM_xmlhttpRequest === "function" ? GM_xmlhttpRequest : null;
  const transportName = gmRequest ? "GM_xmlhttpRequest (CORS/CSP bypass)" : "page fetch";

  if (pageWindow.__TURU_RC_BRIDGE__) {
    pageWindow.__TURU_RC_BRIDGE__.stop();
    console.log("[TURU.RC] previous bridge stopped");
  }

  let stopped = false;
  let timer = null;
  let lastAccess = "";
  let failures = 0;
  let warnedNoToken = false;

  function request(url, options) {
    if (!gmRequest) return fetch(url, options);

    return new Promise((resolve, reject) => {
      gmRequest({
        method: options.method || "GET",
        url,
        headers: options.headers,
        data: options.body,
        timeout: 15000,
        onload(response) {
          const text = response.responseText || "";
          resolve({
            ok: response.status >= 200 && response.status < 300,
            status: response.status,
            text: async () => text,
            json: async () => JSON.parse(text || "{}")
          });
        },
        onerror(error) {
          reject(
            new Error(
              (error && (error.error || error.message)) ||
                "GM_xmlhttpRequest failed (check the @connect permission)"
            )
          );
        },
        ontimeout() {
          reject(new Error("GM_xmlhttpRequest timed out"));
        }
      });
    });
  }

  async function sync() {
    if (stopped) return;

    let accessToken;
    let refreshToken;
    try {
      accessToken = storage.getItem("token");
      refreshToken = storage.getItem("refreshToken");
    } catch (error) {
      console.warn("[TURU.RC] cannot read page localStorage", error);
      return;
    }

    if (!accessToken) {
      if (!warnedNoToken) {
        console.warn("[TURU.RC] no access token found in localStorage; log in to Rollercoin.");
        warnedNoToken = true;
      }
      return;
    }
    warnedNoToken = false;

    try {
      const response = await request(WORKER_URL + "/api/session/push", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: "Bearer " + WORKER_TOKEN
        },
        body: JSON.stringify({
          accessToken: accessToken,
          refreshToken: refreshToken || "",
          source: "browser-bridge"
        }),
        cache: "no-store"
      });

      if (!response.ok) {
        failures += 1;
        if (failures === 1 || failures % 10 === 0) {
          console.warn("[TURU.RC] worker rejected the sync (HTTP " + response.status + ")");
        }
        return;
      }

      const data = await response.json();
      const nextAccess = data && data.session && data.session.accessToken;
      const nextRefresh = data && data.session && data.session.refreshToken;

      if (nextAccess && nextAccess !== accessToken) {
        storage.setItem("token", nextAccess);
      }
      if (nextRefresh && nextRefresh !== refreshToken) {
        storage.setItem("refreshToken", nextRefresh);
      }

      if (nextAccess && nextAccess !== lastAccess) {
        lastAccess = nextAccess;
        console.log(
          "[TURU.RC] token synced to worker" +
            (data.accepted ? "" : " (worker kept its newer token)")
        );
      }
      failures = 0;
    } catch (error) {
      failures += 1;
      if (failures === 1 || failures % 10 === 0) {
        console.warn(
          "[TURU.RC] cannot reach worker at " +
            WORKER_URL +
            " via " +
            transportName +
            ". Is it running? For console mode, a CORS/CSP block is likely — use the userscript mode instead.",
          error && error.message ? error.message : error
        );
      }
    }
  }

  pageWindow.__TURU_RC_BRIDGE__ = {
    stop() {
      stopped = true;
      if (timer) clearInterval(timer);
      timer = null;
      console.log("[TURU.RC] bridge stopped");
    },
    sync
  };

  console.log(
    "%c[TURU.RC] TOKEN BRIDGE ONLINE",
    "color:#b8ff3c;background:#05070f;font-weight:bold;padding:2px 6px"
  );
  console.log(
    "[TURU.RC] transport: " +
      transportName +
      " · syncing every " +
      INTERVAL_MS / 1000 +
      "s · run __TURU_RC_BRIDGE__.stop() to stop"
  );

  sync();
  timer = setInterval(sync, INTERVAL_MS);
})();
