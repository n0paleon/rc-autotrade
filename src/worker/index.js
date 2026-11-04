import { loadConfig } from "./config.js";
import { createLogger } from "./logger.js";
import { Store } from "./store.js";
import { RollercoinApi } from "./api.js";
import { createHttpDispatcher } from "./http.js";
import { KeepaliveService } from "./keepalive.js";
import { SessionManager } from "./session.js";
import { RollercoinGateway } from "./gateway.js";
import { MockUpstream, createMockSession } from "./mock.js";
import { Engine } from "./engine.js";
import { ItemInfoService } from "./item-info.js";
import { ControlServer } from "./server.js";
import { LatencyTracker } from "./latency.js";
import { APP_NAME, APP_VERSION } from "../shared/constants.js";

const MOCK_TARGETS = [
  {
    itemId: "6571aa39ca8bfef34adcd9e9",
    label: "NEON SHARD",
    itemType: "mutation_component",
    targetPrice: 0.1,
    desiredQty: 10,
    highStockThreshold: 20,
    minAcceptableQty: 1,
    allowPartialOrder: true,
    active: true
  },
  {
    itemId: "66b0a1c2d3e4f5061728394a",
    label: "PIXEL CORE",
    itemType: "mutation_component",
    targetPrice: 0.05,
    desiredQty: 25,
    highStockThreshold: 60,
    minAcceptableQty: 5,
    allowPartialOrder: true,
    active: true
  }
];

function printBanner({ config, serverUrl, tokenInfo, mock }) {
  const mode = mock ? "MOCK / SIMULATION" : "LIVE MARKET";
  const lines = [
    "",
    "  ▄▄▄ TURU.RC // MARKET SNIPER ─────────────────────────────",
    `  build      v${APP_VERSION}`,
    `  mode       ${mode}`,
    `  deck       ${serverUrl}`,
    `  upstream   ${mock ? "built-in simulator" : config.wsUrl}`,
    `  data       ${config.dataDir}`,
    "  ───────────────────────────────────────────────────────────"
  ];
  console.log(lines.join("\n"));

  if (tokenInfo.plaintext) {
    console.log("");
    console.log("  ┌─ ACCESS KEY — SHOWN ONCE, SAVE IT NOW ──────────────────┐");
    console.log(`  │ ${tokenInfo.plaintext}`);
    console.log("  └─────────────────────────────────────────────────────────┘");
    console.log("  Paste this key into the control deck to log in.");
  } else if (tokenInfo.source === "env") {
    console.log(`  access key pinned via WORKER_TOKEN (${tokenInfo.preview})`);
  } else {
    console.log(`  access key stored (${tokenInfo.preview}) — run \`npm run key:reset\` if lost`);
  }

  if (config.host === "0.0.0.0") {
    console.log("  ⚠  HOST=0.0.0.0 exposes the control deck on your network. Keep the key private.");
  }
  console.log("");
}

async function main() {
  // Every runtime file (SQLite db, -wal, -shm, backups) holds credentials or
  // order data; create them 0600 by default instead of relying on chmod alone.
  process.umask(0o077);

  const config = loadConfig();
  const logger = createLogger({ level: config.logLevel });
  const startedAt = Date.now();

  const store = new Store({ dataDir: config.dataDir, logger }).load();
  const tokenInfo = store.ensureWorkerToken(config.workerToken);
  if (tokenInfo.source === "generated") {
    store.flush();
  }

  if (config.mock) {
    if (store.state.configs.length === 0) {
      const now = Date.now();
      store.setConfigs(MOCK_TARGETS.map((target) => ({ ...target, createdAt: now, updatedAt: now })));
      logger.info("mock", "seeded demo targets");
    }
    if (!store.state.session.accessToken) {
      store.setSession(createMockSession(), { source: "mock" });
    }
  }

  let session;
  let api;
  let gateway;
  let dispatcher = null;
  let keepalive = null;
  const latency = new LatencyTracker();

  if (config.mock) {
    const upstream = new MockUpstream({ store, logger, latency });
    gateway = upstream;
    api = upstream;
    session = new SessionManager({ store, api, logger });
  } else {
    if (config.keepAliveMs > 0 && config.keepAliveMs >= config.httpKeepAliveMs) {
      logger.warn(
        "http",
        `RC_KEEPALIVE_MS (${config.keepAliveMs}) should be below RC_HTTP_KEEPALIVE_MS (${config.httpKeepAliveMs}); the pool will close idle sockets before the probe`
      );
    }
    dispatcher = createHttpDispatcher({
      keepAliveMs: config.httpKeepAliveMs,
      connections: config.httpConnections
    });
    session = new SessionManager({ store, api: null, logger });
    api = new RollercoinApi({
      baseUrl: config.apiBase,
      session,
      logger,
      dispatcher,
      latency
    });
    session.api = api;
    gateway = new RollercoinGateway({
      url: config.wsUrl,
      getToken: () => session.getAccessToken(),
      getCookie: () => session.getCookie(),
      logger,
      latency
    });
    keepalive = new KeepaliveService({
      url: config.keepAliveUrl,
      intervalMs: config.keepAliveMs,
      api,
      logger,
      dispatcher,
      latency,
      shouldProbe: () =>
        Boolean(session.getAccessToken()) &&
        !store.state.settings.paused &&
        store.state.configs.some((target) => target.active)
    });
  }

  const itemInfo = new ItemInfoService({ store, api, logger });

  let broadcast = () => {};
  const engine = new Engine({
    store,
    api,
    session,
    logger,
    mock: config.mock,
    broadcast: (type, data) => broadcast(type, data),
    onItemType: (itemId, itemType) => {
      itemInfo
        .enrich(itemId, itemType)
        .catch((error) => logger.warn("item-info", `type-based enrichment failed: ${error?.message || error}`));
    }
  });

  // Show the last known prices immediately after a restart instead of an empty
  // deck. Live frames overwrite these as soon as they arrive.
  engine.hydrateMarket();

  const server = new ControlServer({
    config,
    store,
    session,
    gateway,
    engine,
    itemInfo,
    keepalive,
    latency,
    logger,
    startedAt
  });
  broadcast = (type, data) => server.broadcast(type, data);

  itemInfo.on("updated", () => {
    broadcast("configs", server.configsView());
  });

  session.on("updated", ({ changed, status, reason }) => {
    broadcast("session", status);
    keepalive?.wake(`session ${reason}`);
    if (!changed || gateway.stopped) return;

    // A scheduled token refresh does not invalidate an already-open market
    // socket. Reopening it on every refresh caused a reconnect every 30s.
    const mustReplace = reason === "manual" || reason === "bridge";
    if (gateway.status !== "online" || mustReplace) {
      gateway.reconnect(`session ${reason}`);
    }
  });

  gateway.on("status", (stats) => broadcast("gateway", stats));
  gateway.on("frame", (frame) => engine.handleFrame(frame));

  try {
    await server.start();
  } catch (error) {
    if (error?.code === "EADDRINUSE") {
      throw new Error(
        `Port ${config.port} is already in use. Stop the other worker or start with PORT=8788 npm start.`
      );
    }
    throw error;
  }

  printBanner({ config, serverUrl: server.url, tokenInfo, mock: config.mock });

  engine.start();
  session.start();
  gateway.start();
  keepalive?.start();

  // Resolve missing/stale labels in the background. Item-info is a public
  // marketplace endpoint, so this can run before the socket is online.
  itemInfo
    .enrichMany(store.state.configs)
    .catch((error) => logger.warn("item-info", `startup enrichment failed: ${error?.message || error}`));

  if (!config.mock && !session.getAccessToken()) {
    logger.warn("worker", `no Rollercoin session yet — open ${server.url} and paste a token in the LINK tab`);
  }

  logger.info("worker", `${APP_NAME} v${APP_VERSION} ready on ${server.url}${config.mock ? " (mock)" : ""}`);

  let closing = false;
  async function shutdown(signal) {
    if (closing) return;
    closing = true;
    logger.info("worker", `${signal} received — shutting down`);
    try {
      keepalive?.stop();
      gateway.stop();
      session.stop();
      engine.stop();
      await server.stop();
      dispatcher?.destroy?.();
    } catch (error) {
      logger.error("worker", `shutdown error: ${error?.message || error}`);
    } finally {
      store.close();
      setTimeout(() => process.exit(0), 50).unref?.();
    }
  }

  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));

  process.on("uncaughtException", (error) => {
    logger.error("worker", `uncaught exception: ${error?.stack || error}`);
    store.close();
    process.exit(1);
  });

  process.on("unhandledRejection", (error) => {
    logger.error("worker", `unhandled rejection: ${error?.stack || error}`);
  });
}

main().catch((error) => {
  console.error("Fatal:", error?.message || error);
  process.exit(1);
});
