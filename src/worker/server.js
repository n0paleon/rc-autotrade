import { createServer } from "node:http";
import { readFile, unlink } from "node:fs/promises";
import { extname, join, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocket, WebSocketServer } from "ws";
import {
  APP_NAME,
  APP_VERSION,
  LIMITS,
  LOOT_LOG_MAX_PAGE_SIZE,
  LOOT_LOG_PAGE_SIZE
} from "../shared/constants.js";
import {
  normalizeItemId,
  validateSessionInput,
  validateSettings,
  validateTarget,
  validateTargets
} from "../shared/validation.js";
import {
  BACKUP_MAX_BYTES,
  backupFilename,
  buildBackup,
  createDatabaseSnapshot,
  databaseFilename,
  parseBackup,
  restoreBackup
} from "./backup.js";
import { SCHEMA_VERSION } from "./schema.js";
import { jwtExpiry, previewToken } from "./store.js";

const PUBLIC_DIR = fileURLToPath(new URL("../../dist/", import.meta.url));

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".txt": "text/plain; charset=utf-8"
};

const SECURITY_HEADERS = {
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
  "permissions-policy": "camera=(), microphone=(), geolocation=()"
};

const HTML_CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com",
  "font-src 'self' data: https://fonts.gstatic.com",
  "img-src 'self' data:",
  "connect-src 'self' ws: wss:",
  "media-src 'self' data:",
  "object-src 'none'",
  "base-uri 'none'",
  "frame-ancestors 'none'",
  "form-action 'self'"
].join("; ");

function json(res, status, payload, headers = {}) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    ...SECURITY_HEADERS,
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
    ...headers
  });
  res.end(body);
}

/** Reads a non-negative integer query parameter, clamped to `max`. */
function intParam(params, name, fallback, max = Number.MAX_SAFE_INTEGER) {
  const value = Number(params.get(name));
  if (!Number.isFinite(value) || value < 0) return fallback;
  return Math.min(Math.floor(value), max);
}

function corsHeaders() {
  return {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE, OPTIONS",
    "access-control-allow-headers": "authorization, content-type, cache-control, pragma",
    "access-control-allow-private-network": "true",
    "access-control-max-age": "600"
  };
}

async function readJson(req, limit = LIMITS.bodyBytes) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    let rejected = false;

    req.on("data", (chunk) => {
      if (rejected) return;
      size += chunk.length;
      if (size > limit) {
        rejected = true;
        reject(Object.assign(new Error("Request body is too large."), { status: 413 }));
        return;
      }
      chunks.push(chunk);
    });

    req.on("end", () => {
      if (rejected) return;
      if (chunks.length === 0) return resolve({});
      const text = Buffer.concat(chunks).toString("utf8");
      try {
        const parsed = JSON.parse(text);
        if (!parsed || typeof parsed !== "object") {
          return reject(Object.assign(new Error("Expected a JSON object or array."), { status: 400 }));
        }
        resolve(parsed);
      } catch {
        reject(Object.assign(new Error("Invalid JSON body."), { status: 400 }));
      }
    });

    req.on("error", reject);
  });
}

function bearerToken(req) {
  const header = req.headers.authorization;
  if (typeof header !== "string") return "";
  const match = /^Bearer\s+(.+)$/i.exec(header.trim());
  return match ? match[1].trim() : "";
}

/**
 * Control plane: authenticated JSON API, static dashboard, and a WebSocket
 * event stream. The worker binds to loopback by default; the access key is the
 * only thing standing between a local process and the trading engine.
 */
export class ControlServer {
  constructor({
    config,
    store,
    session,
    gateway,
    engine,
    itemInfo,
    keepalive = null,
    latency = null,
    system = null,
    logger,
    startedAt = Date.now()
  }) {
    this.config = config;
    this.store = store;
    this.session = session;
    this.gateway = gateway;
    this.engine = engine;
    this.itemInfo = itemInfo;
    this.keepalive = keepalive;
    this.latency = latency;
    this.system = system;
    this.logger = logger;
    this.startedAt = startedAt;
    this.mock = Boolean(config.mock);
    this.systemHealthLevel = null;

    this.server = createServer((req, res) => {
      this.#handleHttp(req, res).catch((error) => {
        if (res.headersSent) {
          res.destroy();
          return;
        }
        const status = Number(error?.status) || 500;
        if (status >= 500) this.logger.error("http", `${req.method} ${req.url} failed: ${error?.message || error}`);
        const cors = this.#isCorsPath(req.url) ? corsHeaders() : {};
        json(
          res,
          status,
          { ok: false, error: status >= 500 ? "Internal worker error." : error.message },
          cors
        );
      });
    });

    this.server.keepAliveTimeout = 65_000;
    this.server.headersTimeout = 66_000;

    this.wss = new WebSocketServer({
      noServer: true,
      maxPayload: 16 * 1024,
      handleProtocols: (protocols) => (protocols.has("rcw.v1") ? "rcw.v1" : false)
    });

    this.clients = new Set();
    this.authFailures = new Map();
    this.keepaliveTimer = null;
    this.statsTimer = null;
    this.actualPort = config.port;

    this.server.on("upgrade", (req, socket, head) => this.#handleUpgrade(req, socket, head));
    this.wss.on("connection", (ws, req) => this.#handleSocket(ws, req));
  }

  async start() {
    if (typeof this.logger?.setSink === "function") {
      this.logger.setSink((entry) => this.broadcast("log", entry));
    }

    await new Promise((resolve, reject) => {
      const onError = (error) => {
        this.server.off("listening", onListening);
        reject(error);
      };
      const onListening = () => {
        this.server.off("error", onError);
        resolve();
      };
      this.server.once("error", onError);
      this.server.once("listening", onListening);
      this.server.listen(this.config.port, this.config.host);
    });

    this.actualPort = this.server.address()?.port ?? this.config.port;
    this.keepaliveTimer = setInterval(() => {
      for (const client of this.clients) {
        if (client.isAlive === false) {
          client.terminate();
          continue;
        }
        client.isAlive = false;
        try {
          client.ping();
        } catch {
          /* ignore */
        }
      }
    }, 30_000);
    this.keepaliveTimer.unref?.();

    this.statsTimer = setInterval(() => {
      // Health sampling and transition logging happen even with no clients
      // connected, so a struggling worker still leaves a trace in the log.
      if (this.system) {
        const view = this.system.snapshot();
        this.#trackSystemHealth(view);
        if (this.clients.size > 0) this.broadcast("system", view);
      }
      if (this.clients.size === 0) return;

      this.broadcast("gateway", this.gateway.stats());
      // Keep the latency panel live: samples are recorded passively by the
      // worker (API calls, warm probe, ws handshake/ping), so pushing the
      // latest view here lets the deck update without a manual probe.
      const latency = this.latencyView();
      if (latency) this.broadcast("latency", latency);
    }, 5000);
    this.statsTimer.unref?.();
    return this;
  }

  #trackSystemHealth(view) {
    const level = view?.health?.level || "ok";
    if (level === this.systemHealthLevel) return;
    const previous = this.systemHealthLevel;
    this.systemHealthLevel = level;
    if (level === "critical") {
      this.logger.warn("system", `worker health critical — ${(view.health.reasons || []).join(", ") || "unknown"}`);
    } else if (level === "warn") {
      this.logger.warn("system", `worker health degraded — ${(view.health.reasons || []).join(", ") || "unknown"}`);
    } else if (previous === "warn" || previous === "critical") {
      this.logger.info("system", "worker health recovered");
    }
  }

  async stop() {
    if (this.keepaliveTimer) clearInterval(this.keepaliveTimer);
    if (this.statsTimer) clearInterval(this.statsTimer);
    this.keepaliveTimer = null;
    this.statsTimer = null;

    for (const client of this.clients) {
      try {
        client.close(1001, "worker shutting down");
      } catch {
        /* ignore */
      }
    }
    this.clients.clear();

    await new Promise((resolve) => {
      this.wss.close(() => resolve());
      if (!this.server.listening) return resolve();
      this.server.close(() => resolve());
      setTimeout(resolve, 1500).unref?.();
    });
  }

  get url() {
    const host = this.config.host === "0.0.0.0" ? "127.0.0.1" : this.config.host;
    return `http://${host}:${this.actualPort}`;
  }

  broadcast(type, data) {
    if (this.clients.size === 0) return;
    const payload = JSON.stringify({ type, data, at: Date.now() });
    for (const client of this.clients) {
      if (client.readyState !== WebSocket.OPEN) continue;
      try {
        client.send(payload);
      } catch {
        /* drop slow/broken client */
      }
    }
  }

  configsView() {
    const runtime = this.engine.runtimeSnapshot();
    return this.store.state.configs.map((config) => ({
      ...config,
      runtime: runtime[config.itemId.toLowerCase()] ?? null
    }));
  }

  snapshot() {
    const now = Date.now();
    return {
      now,
      name: APP_NAME,
      version: APP_VERSION,
      startedAt: this.startedAt,
      uptimeMs: now - this.startedAt,
      worker: {
        host: this.config.host,
        port: this.actualPort,
        mock: this.mock,
        logLevel: this.config.logLevel
      },
      gateway: this.gateway.stats(),
      session: this.session.status(),
      latency: this.latency ? this.latency.snapshot() : null,
      system: this.systemView(),
      ...(this.keepalive ? { keepalive: this.keepalive.stats() } : {}),
      settings: { ...this.store.state.settings },
      configs: this.configsView(),
      stats: { ...this.store.state.stats },
      report: this.store.reportView(),
      // Newest page only: the full loot log lives in SQLite and is fetched
      // page-by-page through GET /api/history so the deck stays light.
      history: this.store.state.history.slice(0, LOOT_LOG_PAGE_SIZE),
      historyTotal: this.store.historyCount(),
      pending: this.engine.pendingSnapshot(),
      workerTokenPreview: this.store.state.workerTokenPreview,
      accessKeyPinned: Boolean(this.config.workerToken),
      limits: {
        delayMin: LIMITS.delayMin,
        delayMax: LIMITS.delayMax,
        qtyMax: LIMITS.qtyMax,
        configsMax: LIMITS.configsMax,
        ordersPerMinuteMax: LIMITS.ordersPerMinuteMax
      },
      database: { schemaVersion: SCHEMA_VERSION }
    };
  }

  latencyView() {
    return this.latency ? this.latency.snapshot() : null;
  }

  systemView() {
    return this.system ? this.system.snapshot() : null;
  }

  #send(ws, type, data) {
    if (ws.readyState !== WebSocket.OPEN) return;
    try {
      ws.send(JSON.stringify({ type, data, at: Date.now() }));
    } catch {
      /* ignore */
    }
  }

  #handleSocket(ws, req) {
    // Bun's node:http upgrade socket drops direct writes, so a bad key cannot
    // be answered with an HTTP 403. Accept the handshake and close it with a
    // WebSocket code instead; nothing is sent before this check.
    const auth = req.rcAuth || { authorized: false, token: "" };
    if (!auth.authorized) {
      const limited = this.#authFailure(req);
      this.logger?.warn(
        "auth",
        `control socket rejected (${auth.token ? "invalid key" : "missing key"})${limited ? " — too many attempts" : ""}`
      );
      try {
        // 4001 tells the deck to stop reconnecting; 4003 stays reserved for a
        // key rotation, where the client should reconnect with the new key.
        ws.close(4001, limited ? "too many bad access keys" : auth.token ? "invalid access key" : "missing access key");
      } catch {
        /* socket may already be gone */
      }
      return;
    }

    ws.isAlive = true;
    ws.sessionEpoch = this.store.state.workerTokenHash;
    ws.remote = req.socket.remoteAddress;
    this.clients.add(ws);

    ws.on("pong", () => {
      ws.isAlive = true;
    });

    ws.on("message", (data) => {
      if (data.length > 4096) return;
      let message;
      try {
        message = JSON.parse(data.toString());
      } catch {
        return;
      }
      if (message?.type === "ping") {
        this.#send(ws, "pong", { t: message.t, now: Date.now() });
      } else if (message?.type === "refresh") {
        this.#send(ws, "snapshot", this.snapshot());
      }
    });

    ws.on("close", () => this.clients.delete(ws));
    ws.on("error", () => this.clients.delete(ws));

    this.#send(ws, "hello", {
      name: APP_NAME,
      version: APP_VERSION,
      serverTime: Date.now(),
      recentLogs: this.logger.recent(80)
    });
    this.#send(ws, "snapshot", this.snapshot());
  }

  #handleUpgrade(req, socket, head) {
    let url;
    try {
      url = new URL(req.url, `http://${req.headers.host || "localhost"}`);
    } catch {
      socket.destroy();
      return;
    }

    if (url.pathname !== "/ws") {
      // No HTTP reply is possible on Bun's upgrade socket; a plain close is
      // enough for a client that dialed the wrong path.
      socket.destroy();
      return;
    }

    const protocols = String(req.headers["sec-websocket-protocol"] || "")
      .split(",")
      .map((value) => value.trim())
      .filter(Boolean);
    const offered = protocols.filter((value) => value === "rcw.v1");
    const protocolToken = protocols.find((value) => value !== "rcw.v1") || "";
    const token = protocolToken || url.searchParams.get("token") || "";
    req.rcAuth = {
      token,
      authorized: offered.length > 0 && this.store.verifyWorkerToken(token)
    };

    this.wss.handleUpgrade(req, socket, head, (ws) => {
      this.wss.emit("connection", ws, req);
    });
  }

  #authFailure(req) {
    const ip = req.socket.remoteAddress || "unknown";
    const now = Date.now();
    const entry = this.authFailures.get(ip);
    if (!entry || now > entry.resetAt) {
      this.authFailures.set(ip, { count: 1, resetAt: now + 60_000 });
      return false;
    }
    entry.count += 1;
    return entry.count > 20;
  }

  #isCorsPath(rawUrl) {
    try {
      const path = new URL(rawUrl || "/", "http://localhost").pathname;
      return path.startsWith("/api/");
    } catch {
      return false;
    }
  }

  /**
   * Adds the CORS headers to every API reply without touching the ~20 places
   * that write responses. `writeHead` is wrapped once per response, so existing
   * `json()`/`writeHead()` calls keep working and cannot forget the headers.
   */
  #attachCors(res) {
    const writeHead = res.writeHead.bind(res);
    res.writeHead = (status, ...rest) => {
      const headers = typeof rest[0] === "string" ? { ...rest[1] } : { ...(rest[0] || {}) };
      // A caller that already set its own CORS headers (the preflight handler)
      // keeps them; anything else inherits the defaults.
      for (const [key, value] of Object.entries(corsHeaders())) {
        if (!Object.keys(headers).some((name) => name.toLowerCase() === key)) headers[key] = value;
      }
      const vary = headers.vary ? `${headers.vary}, access-control-request-headers` : "access-control-request-headers";
      headers.vary = vary;
      return typeof rest[0] === "string" ? writeHead(status, rest[0], headers) : writeHead(status, headers);
    };
    return res;
  }

  #authorized(req, res, { cors = false } = {}) {
    const token = bearerToken(req);
    if (token && this.store.verifyWorkerToken(token)) return true;

    const limited = this.#authFailure(req);
    json(
      res,
      limited ? 429 : 401,
      { ok: false, error: limited ? "Too many bad access keys. Try again in a minute." : "Missing or invalid worker access key." },
      {
        "www-authenticate": 'Bearer realm="turu-rc"',
        ...(cors ? corsHeaders() : {})
      }
    );
    return false;
  }

  async #handleHttp(req, res) {
    const method = req.method || "GET";
    let url;
    try {
      url = new URL(req.url || "/", `http://${req.headers.host || "localhost"}`);
    } catch {
      json(res, 400, { ok: false, error: "Bad request URL." });
      return;
    }
    const path = url.pathname;
    const isApi = path.startsWith("/api/");

    // CORS preflight is a browser permission check, not an authenticated call:
    // it never carries the Bearer key, so it has to be answered before the auth
    // gate below. Leaving it to the gate answered 401 without any CORS headers,
    // and the browser then blocked the real request — which broke logging in
    // from a separately served UI (Vite dev server on another origin).
    if (method === "OPTIONS" && isApi) {
      const cors = corsHeaders();
      const requestedHeaders = req.headers["access-control-request-headers"];
      if (typeof requestedHeaders === "string" && requestedHeaders.trim()) {
        // Echo the browser's list so future fetch options (cache-control, etc.)
        // do not break the preflight.
        cors["access-control-allow-headers"] = requestedHeaders
          .split(",")
          .map((value) => value.trim())
          .filter(Boolean)
          .join(", ");
      }
      res.writeHead(204, { ...SECURITY_HEADERS, ...cors, vary: "access-control-request-headers, access-control-request-private-network" });
      res.end();
      return;
    }

    if (path === "/api/health") {
      json(
        res,
        200,
        {
          ok: true,
          name: APP_NAME,
          version: APP_VERSION,
          mock: this.mock,
          uptimeMs: Date.now() - this.startedAt
        },
        corsHeaders()
      );
      return;
    }

    // Every API response needs CORS headers, not just the hand-written
    // preflight and the two bridge endpoints: a separately served UI (Vite dev
    // server on another origin) calls the whole /api surface, and without these
    // headers the browser discards an otherwise successful 200.
    if (isApi) this.#attachCors(res);

    if (isApi && !this.#authorized(req, res, { cors: this.#isCorsPath(path) })) return;

    if (!isApi) {
      if (method !== "GET" && method !== "HEAD") {
        json(res, 405, { ok: false, error: "Method not allowed." });
        return;
      }
      await this.#serveStatic(path, req, res);
      return;
    }

    if (method === "GET" && path === "/api/history") {
      const page = this.store.historyPage({
        q: url.searchParams.get("q") || "",
        status: url.searchParams.get("status") || "",
        limit: intParam(url.searchParams, "limit", LOOT_LOG_PAGE_SIZE, LOOT_LOG_MAX_PAGE_SIZE),
        offset: intParam(url.searchParams, "offset", 0),
        beforeAt: intParam(url.searchParams, "beforeAt", 0),
        beforeId: url.searchParams.get("beforeId") || ""
      });
      json(res, 200, { ok: true, ...page });
      return;
    }

    if (method === "GET" && path === "/api/state") {
      json(res, 200, { ok: true, ...this.snapshot() });
      return;
    }

    if (method === "GET" && path === "/api/backup") {
      const format = (url.searchParams.get("format") || "json").toLowerCase();
      if (format === "sqlite" || format === "db") {
        // A consistent copy without stopping the worker. The temp file is
        // removed as soon as it has been read into the response.
        const file = createDatabaseSnapshot(this.store, { prefix: "export" });
        let data;
        try {
          data = await readFile(file);
        } finally {
          await unlink(file).catch(() => {});
        }
        res.writeHead(200, {
          ...SECURITY_HEADERS,
          "content-type": "application/vnd.sqlite3",
          "content-disposition": `attachment; filename="${databaseFilename()}"`,
          "cache-control": "no-store",
          "content-length": data.length
        });
        res.end(data);
        return;
      }
      if (format !== "json") {
        json(res, 400, { ok: false, error: "format must be json, sqlite or db." });
        return;
      }

      const credentials = url.searchParams.get("credentials") || "";
      const includeCredentials = /^(1|true|yes)$/i.test(credentials);
      const backup = buildBackup(this.store, { includeCredentials });
      const body = JSON.stringify(backup);
      res.writeHead(200, {
        ...SECURITY_HEADERS,
        "content-type": "application/json; charset=utf-8",
        "content-disposition": `attachment; filename="${backupFilename()}"`,
        "cache-control": "no-store",
        "content-length": Buffer.byteLength(body)
      });
      res.end(body);
      return;
    }

    if (method === "POST" && path === "/api/backup") {
      const body = await readJson(req, BACKUP_MAX_BYTES);
      const parsed = parseBackup(body);
      if (!parsed.ok) {
        json(res, 422, {
          ok: false,
          error: "This file is not a valid backup for this worker.",
          fields: parsed.errors
        });
        return;
      }

      const credentialsParam = url.searchParams.get("credentials");
      const restoreCredentials = credentialsParam == null || !/^(0|false|no)$/i.test(credentialsParam);
      const previousToken = this.store.state.session.accessToken;

      let result;
      try {
        result = restoreBackup(this.store, parsed.value, { restoreCredentials });
      } catch (error) {
        this.logger.error("backup", `restore failed: ${error?.message || error}`);
        throw error;
      }

      // The restored rows replaced every target and the market cache, so the
      // engine's live copy of that world is no longer valid.
      this.engine?.resetMarketState?.();
      this.engine?.hydrateMarket?.();
      for (const config of this.store.state.configs) {
        this.engine?.refreshTrend?.(config.itemId);
      }
      this.session?.tick?.();
      this.keepalive?.wake("backup restored");

      const tokenChanged = previousToken !== this.store.state.session.accessToken;
      if (restoreCredentials && result.credentialsRestored && tokenChanged && this.gateway?.status === "online") {
        this.gateway.reconnect("backup restored");
      }

      this.logger.info(
        "backup",
        `restored ${result.counts.targets} targets and ${result.counts.orders} loot entries` +
          (result.safetyBackup ? ` — safety snapshot ${result.safetyBackup}` : "")
      );

      // LootLog keeps its own paging state in the browser. Clearing it first,
      // then handing over the restored snapshot, guarantees no pre-import row
      // survives the swap.
      this.broadcast("history-cleared", {});
      this.broadcast("snapshot", this.snapshot());

      const { safetyBackupPath, ...restored } = result;
      json(res, 200, { ok: true, restored, warnings: parsed.warnings });
      return;
    }

    if (method === "GET" && path === "/api/report") {
      json(res, 200, { ok: true, report: this.store.reportView() });
      return;
    }

    if (method === "GET" && path === "/api/session") {
      json(res, 200, { ok: true, session: this.session.status() });
      return;
    }

    if (method === "DELETE" && path === "/api/session") {
      const status = this.session.clear();
      this.keepalive?.wake("session cleared");
      this.logger.info("session", "Rollercoin session cleared from the control deck");
      json(res, 200, { ok: true, session: status });
      return;
    }

    if (method === "POST" && path === "/api/session") {
      const body = await readJson(req);
      const result = validateSessionInput(body);
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Invalid session data.", fields: result.errors });
        return;
      }
      if (Object.keys(result.value).length === 0) {
        json(res, 400, { ok: false, error: "Nothing to update." });
        return;
      }
      const before = this.session.getAccessToken();
      const status = this.session.set({ ...result.value, source: "ui" });
      const changed = before !== this.session.getAccessToken();
      this.keepalive?.wake("session updated");
      json(res, 200, { ok: true, changed, session: status });
      return;
    }

    if (method === "POST" && path === "/api/session/push") {
      const body = await readJson(req);
      const result = validateSessionInput(body);
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Invalid session data.", fields: result.errors }, corsHeaders());
        return;
      }
      if (!result.value.accessToken) {
        json(res, 400, { ok: false, error: "accessToken is required." }, corsHeaders());
        return;
      }
      const merged = this.#mergePushedSession(result.value);
      json(
        res,
        200,
        {
          ok: true,
          accepted: merged.accepted,
          session: {
            accessToken: this.store.state.session.accessToken,
            refreshToken: this.store.state.session.refreshToken
          }
        },
        corsHeaders()
      );
      return;
    }

    if (method === "POST" && path === "/api/session/refresh") {
      const refreshed = await this.session.refresh("manual", { force: true });
      json(res, 200, { ok: true, refreshed, session: this.session.status() });
      return;
    }

    if (method === "POST" && path === "/api/auth/rotate") {
      if (this.config.workerToken) {
        json(res, 409, {
          ok: false,
          error: "Access key is pinned by WORKER_TOKEN. Change the environment variable to rotate it."
        });
        return;
      }
      const token = this.store.rotateWorkerToken();
      this.logger.warn("auth", "worker access key rotated; revoking control sockets");
      json(res, 200, { ok: true, token, preview: previewToken(token) });
      setTimeout(() => {
        for (const client of this.clients) {
          try {
            client.close(4003, "access-key-rotated");
          } catch {
            /* ignore */
          }
        }
      }, 350).unref?.();
      return;
    }

    if (method === "GET" && path === "/api/item-info") {
      const itemId = normalizeItemId(url.searchParams.get("itemId") || "");
      if (!itemId) {
        json(res, 422, { ok: false, error: "A valid itemId is required." });
        return;
      }
      const itemType = (url.searchParams.get("itemType") || "mutation_component").toLowerCase();
      if (!/^[a-z][a-z0-9_-]{1,39}$/.test(itemType)) {
        json(res, 422, { ok: false, error: "Invalid itemType." });
        return;
      }
      if (!this.itemInfo) {
        json(res, 503, { ok: false, error: "Item-info service is not available." });
        return;
      }
      const configured = this.store.state.configs.some(
        (candidate) => candidate.itemId.toLowerCase() === itemId.toLowerCase()
      );
      const result = configured
        ? await this.itemInfo.enrich(itemId, itemType)
        : await this.itemInfo.preview(itemId, itemType);
      if (!result.ok) {
        json(res, 502, { ok: false, error: result.error || "Could not resolve item info." });
        return;
      }
      json(res, 200, { ok: true, item: result.value });
      return;
    }

    if (method === "GET" && path === "/api/configs") {
      json(res, 200, { ok: true, configs: this.configsView() });
      return;
    }

    if (method === "PUT" && path === "/api/configs") {
      const body = await readJson(req);
      const list = Array.isArray(body) ? body : body.configs;
      const result = validateTargets(list);
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Some targets are invalid.", fields: result.errors });
        return;
      }
      this.store.setConfigs(result.values);
      for (const config of result.values) this.engine.refreshTrend(config.itemId);
      this.logger.info("config", `target list replaced (${result.values.length} item${result.values.length === 1 ? "" : "s"})`);
      this.keepalive?.wake("target list replaced");
      this.broadcast("configs", this.configsView());
      json(res, 200, { ok: true, configs: this.configsView() });
      this.itemInfo
        ?.enrichMany(result.values)
        .catch((error) => this.logger.warn("item-info", `bulk enrichment failed: ${error?.message || error}`));
      return;
    }

    if (method === "POST" && path === "/api/configs") {
      const body = await readJson(req);
      const existingIds = this.store.state.configs.map((config) => config.itemId);
      const result = validateTarget(body, { existingIds });
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Target is invalid.", fields: result.errors });
        return;
      }
      const saved = this.store.upsertConfig(result.value);
      this.engine.refreshTrend(saved.itemId);
      this.logger.info("config", `target added: ${saved.itemId}`);
      this.keepalive?.wake("target added");
      if (this.itemInfo) {
        // Resolve the auto label before replying when possible. The timeout
        // keeps a slow item-info endpoint from blocking target creation.
        await Promise.race([
          this.itemInfo.enrich(saved.itemId, saved.itemType),
          new Promise((resolve) => setTimeout(resolve, 1500))
        ]).catch((error) =>
          this.logger.warn("item-info", `enrichment failed: ${error?.message || error}`)
        );
      }
      this.broadcast("configs", this.configsView());
      json(res, 201, { ok: true, configs: this.configsView() });
      return;
    }

    const itemMatch = /^\/api\/configs\/([^/]+)$/.exec(path);
    if (itemMatch && (method === "PATCH" || method === "DELETE")) {
      const itemId = decodeURIComponent(itemMatch[1]);
      const existing = this.store.state.configs.find(
        (config) => config.itemId.toLowerCase() === itemId.toLowerCase()
      );
      if (!existing) {
        json(res, 404, { ok: false, error: "Target not found." });
        return;
      }

      if (method === "DELETE") {
        this.store.removeConfig(existing.itemId);
        this.logger.info("config", `target removed: ${existing.label || existing.itemId}`);
        this.broadcast("configs", this.configsView());
        json(res, 200, { ok: true, configs: this.configsView() });
        return;
      }

      const body = await readJson(req);
      const merged = { ...existing, ...body, itemId: existing.itemId };
      const result = validateTarget(merged, { originalId: existing.itemId });
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Target is invalid.", fields: result.errors });
        return;
      }
      const saved = this.store.upsertConfig(result.value, { originalId: existing.itemId });
      this.engine.refreshTrend(saved.itemId);
      this.logger.info("config", `target updated: ${saved.itemId}`);
      this.keepalive?.wake("target updated");
      this.broadcast("configs", this.configsView());
      json(res, 200, { ok: true, configs: this.configsView() });
      this.itemInfo
        ?.enrich(saved.itemId, saved.itemType)
        .catch((error) => this.logger.warn("item-info", `enrichment failed: ${error?.message || error}`));
      return;
    }

    if (method === "PATCH" && path === "/api/settings") {
      const body = await readJson(req);
      const result = validateSettings(body);
      if (!result.ok) {
        json(res, 422, { ok: false, error: "Some settings are invalid.", fields: result.errors });
        return;
      }
      this.store.patchSettings(result.value);
      if (typeof result.value.autoRefresh === "boolean" && result.value.autoRefresh) {
        this.session.tick();
      }
      if (result.value.paused === false) this.keepalive?.wake("trading resumed");
      this.logger.info("config", "runtime settings updated");
      this.broadcast("settings", this.store.state.settings);
      json(res, 200, { ok: true, settings: this.store.state.settings });
      return;
    }

    if (method === "POST" && path === "/api/control") {
      const body = await readJson(req);
      if (body.action === "pause" || body.action === "resume" || body.action === "toggle") {
        const paused =
          body.action === "toggle"
            ? !this.store.state.settings.paused
            : body.action === "pause";
        this.store.patchSettings({ paused });
        this.logger.info("control", paused ? "trading paused" : "trading resumed");
        if (!paused) this.keepalive?.wake("trading resumed");
        this.broadcast("settings", this.store.state.settings);
        json(res, 200, { ok: true, settings: this.store.state.settings });
        return;
      }
      if (body.action === "reconnect") {
        this.gateway.reconnect("manual");
        json(res, 200, { ok: true });
        return;
      }
      json(res, 400, { ok: false, error: "action must be pause, resume, toggle or reconnect." });
      return;
    }

    if (method === "GET" && path === "/api/latency") {
      json(res, 200, { ok: true, latency: this.latencyView() });
      return;
    }

    if (method === "GET" && path === "/api/system") {
      if (!this.system) {
        json(res, 503, { ok: false, error: "System monitoring is not available." });
        return;
      }
      json(res, 200, { ok: true, system: this.systemView() });
      return;
    }

    if (method === "POST" && path === "/api/latency/probe") {
      if (!this.latency) {
        json(res, 503, { ok: false, error: "Latency tracking is not available." });
        return;
      }
      const out = { ok: true, latency: this.latencyView(), probe: null };
      // Manual probe: run the same warm GET on demand so the user can get a
      // fresh sample without waiting for the scheduled keepalive interval.
      // Mock mode has no keepalive, so it reports `skipped`.
      if (this.keepalive?.enabled) {
        const probe = await this.keepalive.probeOnce();
        out.probe = { ok: Boolean(probe?.ok), status: probe?.status ?? null, error: probe?.error ?? null };
        out.latency = this.latencyView();
      } else {
        out.probe = { ok: false, skipped: true, reason: "keepalive disabled" };
      }
      this.broadcast("latency", out.latency);
      json(res, 200, out);
      return;
    }

    if (method === "DELETE" && path === "/api/history") {
      this.store.resetStats();
      this.engine.resetCounters();
      this.latency?.reset();
      this.logger.info("stats", "lifetime stats, per-target fill rates and loot log reset");
      this.broadcast("stats", this.store.state.stats);
      this.broadcast("report", this.store.reportView());
      this.broadcast("latency", this.latencyView());
      this.broadcast("configs", this.configsView());
      this.broadcast("history-cleared", {});
      json(res, 200, { ok: true, stats: this.store.state.stats });
      return;
    }

    json(res, 404, { ok: false, error: "Not found." });
  }

  #mergePushedSession(incoming) {
    const session = this.store.state.session;
    const current = session.accessToken;
    const now = Date.now();
    const currentExp = jwtExpiry(current);
    const incomingExp = jwtExpiry(incoming.accessToken);

    let accept = false;
    if (!current) accept = true;
    else if (incoming.accessToken === current) {
      // Same access token: still allow a missing refresh token to be filled in.
      if (!session.refreshToken && incoming.refreshToken) {
        this.session.set({ refreshToken: incoming.refreshToken, source: "bridge" });
      }
      return { accepted: false };
    }
    else if (currentExp && currentExp <= now) accept = true;
    else if (incomingExp && currentExp) accept = incomingExp > currentExp + 30_000;
    else if (incomingExp && !currentExp) accept = true;
    else if (!incomingExp && currentExp) accept = false;
    else accept = now - (session.updatedAt || 0) > 20 * 60_000;

    if (!accept) return { accepted: false };

    const patch = {
      accessToken: incoming.accessToken,
      source: "bridge"
    };
    if (incoming.refreshToken) patch.refreshToken = incoming.refreshToken;
    this.session.set(patch);
    this.keepalive?.wake("bridge session");
    return { accepted: true };
  }

  async #serveStatic(path, req, res) {
    let relative = path === "/" ? "index.html" : decodeURIComponent(path.replace(/^\/+/, ""));
    relative = normalize(relative).replace(/^(\.\.(\/|\\|$))+/, "");
    const filePath = join(PUBLIC_DIR, relative);

    if (!filePath.startsWith(PUBLIC_DIR.endsWith(sep) ? PUBLIC_DIR : `${PUBLIC_DIR}${sep}`)) {
      json(res, 403, { ok: false, error: "Forbidden." });
      return;
    }

    let data;
    try {
      data = await readFile(filePath);
    } catch {
      if (!extname(relative)) {
        try {
          data = await readFile(join(PUBLIC_DIR, "index.html"));
          relative = "index.html";
        } catch {
          json(res, 404, { ok: false, error: "Not found." });
          return;
        }
      } else {
        json(res, 404, { ok: true, error: "Not found." });
        return;
      }
    }

    const type = MIME[extname(relative).toLowerCase()] || "application/octet-stream";
    const headers = {
      ...SECURITY_HEADERS,
      "content-type": type,
      "cache-control": "no-cache",
      "content-length": data.length
    };
    if (relative.endsWith(".html")) headers["content-security-policy"] = HTML_CSP;
    res.writeHead(200, headers);
    if (req.method === "HEAD") res.end();
    else res.end(data);
  }
}
