import { EventEmitter } from "node:events";
import { randomBytes } from "node:crypto";

function makeToken(ttlSeconds = 3600) {
  const header = Buffer.from(JSON.stringify({ alg: "none", typ: "JWT" })).toString("base64url");
  const payload = Buffer.from(
    JSON.stringify({
      sub: "mock-player",
      role: "mock",
      iat: Math.floor(Date.now() / 1000),
      exp: Math.floor(Date.now() / 1000) + ttlSeconds
    })
  ).toString("base64url");
  return `${header}.${payload}.${randomBytes(24).toString("base64url")}`;
}

export function createMockSession() {
  return {
    accessToken: makeToken(3600),
    refreshToken: `mock-refresh.${randomBytes(18).toString("base64url")}`,
    source: "mock"
  };
}

function round6(value) {
  return Math.round(value * 1e6) / 1e6;
}

/**
 * Development-only upstream. It speaks the same frame shapes as Rollercoin so
 * the engine, API, socket layer and UI can be exercised end-to-end offline.
 */
export class MockUpstream extends EventEmitter {
  constructor({ store, logger, latency = null, now = () => Date.now() }) {
    super();
    this.store = store;
    this.logger = logger;
    this.latency = latency;
    this.now = now;
    this.status = "idle";
    this.connectedAt = null;
    this.lastMessageAt = null;
    this.lastError = null;
    this.attempts = 0;
    this.reconnects = 0;
    this.eventCount = 0;
    this.parseErrors = 0;
    this.cmdCounts = new Map();
    this.tickTimer = null;
    this.ticks = 0;
    this.connectStartedAt = null;
    this.pingTimer = null;
    this.lastPingMs = null;
    const interval = Number.parseInt(process.env.MOCK_INTERVAL_MS || "", 10);
    this.tickMs = Number.isInteger(interval) ? Math.max(200, Math.min(10_000, interval)) : 1400;
    this.orderSeq = 0;
    this.priceWalk = new Map();
    this.orderTimers = new Set();
    this.lastRequestAt = 0;
  }

  start() {
    if (this.tickTimer) return;
    this.status = "connecting";
    this.connectStartedAt = this.now();
    this.emit("status", this.stats());
    setTimeout(() => {
      if (this.status === "connecting") {
        this.status = "online";
        this.connectedAt = this.now();
        if (this.latency) {
          this.latency.record("wsHandshake", this.now() - this.connectStartedAt, { ok: true });
          // Simulated WS ping/pong every ~10s so the UI gets a live ping sample.
          this.pingTimer = setInterval(() => {
            const ms = 18 + Math.round(Math.random() * 22);
            this.lastPingMs = ms;
            this.latency.record("wsPing", ms, { ok: true });
          }, 10_000);
          this.pingTimer.unref?.();
          // Seed an immediate sample so the HUD is never blank in mock mode.
          this.latency.record("wsPing", 24, { ok: true });
        }
        this.reconnects += 1;
        this.emit("status", this.stats());
        this.logger.info("mock", "simulated market feed online");
      }
    }, 350).unref?.();
    this.tickTimer = setInterval(() => this.#tick(), this.tickMs);
    this.tickTimer.unref?.();
  }

  stop() {
    if (this.tickTimer) clearInterval(this.tickTimer);
    this.tickTimer = null;
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.pingTimer = null;
    for (const timer of this.orderTimers) clearTimeout(timer);
    this.orderTimers.clear();
    this.status = "stopped";
    this.connectedAt = null;
    this.emit("status", this.stats());
  }

  reconnect(reason = "manual") {
    this.logger.info("mock", `reconnect requested: ${reason}`);
    if (this.tickTimer) clearInterval(this.tickTimer);
    this.tickTimer = null;
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.pingTimer = null;
    this.status = "connecting";
    this.connectStartedAt = this.now();
    this.emit("status", this.stats());
    setTimeout(() => {
      this.status = "online";
      this.connectedAt = this.now();
      if (this.latency) {
        this.latency.record("wsHandshake", this.now() - this.connectStartedAt, { ok: true });
      }
      this.reconnects += 1;
      this.emit("status", this.stats());
      this.tickTimer = setInterval(() => this.#tick(), this.tickMs);
      this.tickTimer.unref?.();
    }, 300).unref?.();
  }

  #record(cmd) {
    this.eventCount += 1;
    this.lastMessageAt = Date.now();
    this.cmdCounts.set(cmd, (this.cmdCounts.get(cmd) || 0) + 1);
  }

  #emitFrame(cmd, value) {
    this.#record(cmd);
    this.emit("frame", { cmd, value, raw: { cmd, value }, at: Date.now() });
  }

  #tick() {
    if (this.status !== "online") return;
    this.ticks += 1;

    const configs = this.store.state.configs;
    for (const config of configs) this.#emitMarket(config);

    // Noise: unrelated marketplace traffic, so we can prove the worker filters.
    if (this.ticks % 4 === 0) {
      this.#emitFrame("marketplace_orders_update", {
        item_id: `65aa${randomBytes(8).toString("hex")}`,
        currency: "RLT",
        item_type: "mutation_component",
        data: { list: [{ price: 250_000, quantity: 4 }] }
      });
    }

    if (this.ticks % 6 === 0) {
      this.#emitFrame("marketplace_activity", {
        item_id: `65aa${randomBytes(8).toString("hex")}`,
        currency: "RLT",
        data: { list: [] }
      });
    }
  }

  #emitMarket(config) {
    const key = config.itemId;
    const walk = this.priceWalk.get(key) || { factor: 1.05, stock: config.desiredQty };
    const phase = this.ticks % 9;
    const target = config.targetPrice;

    let factor;
    if (phase === 3) {
      // Guaranteed discount phase so demos and tests always see a buy.
      factor = 0.87 + Math.random() * 0.05;
      walk.factor = factor;
    } else if (phase === 4) {
      factor = 0.93 + Math.random() * 0.04;
      walk.factor = factor;
    } else {
      factor = 0.99 + Math.random() * 0.22;
      walk.factor = walk.factor * 0.55 + factor * 0.45;
    }

    if (phase === 3 || phase === 4) {
      walk.stock = Math.max(config.minAcceptableQty, config.desiredQty + Math.floor(Math.random() * config.highStockThreshold));
    } else if (Math.random() < 0.12) {
      walk.stock = 0;
    } else {
      walk.stock = Math.floor(Math.random() * Math.max(config.desiredQty * 3, config.highStockThreshold + 1));
    }

    this.priceWalk.set(key, walk);

    const bestPrice = Math.max(0.000001, round6(target * walk.factor));
    const bestRaw = Math.round(bestPrice * 1e6);
    const list = [
      { price: bestRaw, quantity: walk.stock },
      { price: Math.round(target * 1.06 * 1e6), quantity: Math.max(1, Math.floor(walk.stock * 0.6)) },
      { price: Math.round(target * 1.14 * 1e6), quantity: Math.max(1, Math.floor(walk.stock * 0.4)) }
    ].filter((entry) => entry.quantity > 0);

    this.#emitFrame("marketplace_orders_update", {
      item_id: config.itemId,
      currency: "RLT",
      item_type: "mutation_component",
      data: {
        list,
        allowedPriceMin: Math.max(1, Math.round(bestRaw * 0.5)),
        allowedPriceMax: bestRaw * 6
      }
    });
  }

  async #timedHttp(run) {
    return this.#timedHttpImpl(run);
  }

  async #timedHttpImpl(run) {
    this.lastRequestAt = this.now();
    if (!this.latency) return run();
    return this.latency.measure("http", run, { okFn: () => true });
  }

  async purchase({ itemId, qty, unitPriceRaw }) {
    return this.#timedHttp(() => this.#purchase({ itemId, qty, unitPriceRaw }));
  }

  async #purchase({ itemId, qty, unitPriceRaw }) {
    if (this.status !== "online") {
      return { ok: false, status: 503, error: "Mock market feed is offline." };
    }
    this.orderSeq += 1;
    const orderId = `mock_${Date.now().toString(36)}_${this.orderSeq}`;
    const failed = Math.random() < 0.08;
    const delay = 500 + Math.random() * 900;

    const timer = setTimeout(() => {
      this.orderTimers.delete(timer);
      if (failed) {
        this.#emitFrame("marketplace_orders_filled", {
          item_id: itemId,
          order_id: orderId,
          count: 0,
          price: 0,
          status: 2
        });
      } else {
        this.#emitFrame("marketplace_orders_filled", {
          item_id: itemId,
          order_id: orderId,
          count: qty,
          price: unitPriceRaw * qty,
          status: 12
        });
      }
    }, delay);
    this.orderTimers.add(timer);
    timer.unref?.();

    return { ok: true, status: 200, orderId, body: { success: true } };
  }

  async fetchItemInfo({ itemId, itemType = "mutation_component" }) {
    return this.#timedHttp(() => this.#fetchItemInfo({ itemId, itemType }));
  }

  async #fetchItemInfo({ itemId, itemType = "mutation_component" }) {
    const known = {
      "6571aa39ca8bfef34adcd9e9": { name: "Neon Shard", rarity: "Rare", color: "b8ff3c" },
      "66b0a1c2d3e4f5061728394a": { name: "Pixel Core", rarity: "Epic", color: "ff5fc8" }
    };
    const entry = known[String(itemId).toLowerCase()] || {
      name: `Component ${String(itemId).slice(-4).toUpperCase()}`,
      rarity: "Uncommon",
      color: "2bd600"
    };
    return {
      ok: true,
      status: 200,
      data: {
        _id: itemId,
        name: { en: entry.name },
        itemType,
        rarityGroup: {
          title: { en: entry.rarity },
          baseHexColor: entry.color
        }
      }
    };
  }

  async refreshSession(refreshToken) {
    return this.#timedHttp(() => this.#refreshSession(refreshToken));
  }

  async #refreshSession(refreshToken) {
    if (!refreshToken) {
      return { ok: false, status: 400, error: "Mock session has no refresh token." };
    }
    await new Promise((resolve) => setTimeout(resolve, 120).unref?.());
    return {
      ok: true,
      status: 200,
      accessToken: makeToken(3600),
      refreshToken: `mock-refresh.${randomBytes(18).toString("base64url")}`
    };
  }

  getLastRequestAt() {
    return this.lastRequestAt;
  }

  stats() {
    const commands = [...this.cmdCounts.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 15)
      .map(([cmd, count]) => ({ cmd, count }));

    return {
      status: this.status,
      connectedAt: this.connectedAt,
      lastMessageAt: this.lastMessageAt,
      lastError: this.lastError,
      attempts: this.attempts,
      reconnects: this.reconnects,
      eventCount: this.eventCount,
      parseErrors: this.parseErrors,
      commands,
      upstream: "simulation",
      uptimeMs: this.connectedAt ? this.now() - this.connectedAt : 0,
      mock: true,
      lastPingMs: this.lastPingMs
    };
  }
}
