import { randomUUID } from "node:crypto";
import {
  MARKET_SNAPSHOT_THROTTLE_MS,
  ORDER_STATUS,
  PRICE_HISTORY_LIMIT,
  PRICE_POINT_WRITE_INTERVAL_MS
} from "../shared/constants.js";
import { normalizeUpstreamEvent } from "./events.js";
import { planOrder, roundPrice } from "../shared/rules.js";

const PENDING_TIMEOUT_MS = 120_000;
const SCAN_LOG_INTERVAL_MS = 12_000;

function labelFor(config, itemId) {
  return config?.label || `ITEM ${String(itemId || "?").slice(-6).toUpperCase()}`;
}

/**
 * The matching + execution brain.
 *
 * Every socket frame is normalised, filtered against the target list, and the
 * pure `planOrder` rules decide whether an order is worth sending. HTTP work is
 * fire-and-forget from the frame loop so a slow marketplace call never blocks
 * subsequent price ticks.
 */
export class Engine {
  constructor({
    store,
    api,
    session,
    logger,
    mock = false,
    broadcast = () => {},
    onItemType = () => {},
    clock = () => Date.now()
  }) {
    this.store = store;
    this.api = api;
    this.session = session;
    this.logger = logger;
    this.mock = mock;
    this.broadcast = broadcast;
    this.onItemType = onItemType;
    this.clock = clock;

    this.runtime = new Map();
    this.pending = new Map();
    this.lastOrderAt = 0;
    this.orderStamps = [];
    this.cleanupTimer = null;
    this.lastNoTokenWarnAt = 0;
  }

  start() {
    if (this.cleanupTimer) return;
    this.cleanupTimer = setInterval(() => this.#prunePending(), 15_000);
    this.cleanupTimer.unref?.();
  }

  stop() {
    if (this.cleanupTimer) clearInterval(this.cleanupTimer);
    this.cleanupTimer = null;
    this.flushMarketCache();
  }

  /**
   * Rebuilds the display cache from SQLite after a restart.
   *
   * This is presentation-only: `planOrder` still runs exclusively inside
   * `#handleMarket` on a live socket frame, so a snapshot from three hours ago
   * can light up the deck but can never trigger a purchase.
   */
  hydrateMarket() {
    if (typeof this.store.loadMarketSnapshots !== "function") return 0;
    const snapshots = this.store.loadMarketSnapshots();
    let restored = 0;

    for (const [itemId, snapshot] of Object.entries(snapshots)) {
      if (snapshot?.seenAt == null) continue;
      const config = this.#configFor(itemId);
      if (!config) continue;

      const runtime = this.#runtime(itemId);
      if (Number.isFinite(snapshot.lastPrice)) runtime.lastPrice = snapshot.lastPrice;
      if (Number.isFinite(snapshot.lastStock)) runtime.lastStock = snapshot.lastStock;
      if (Number.isFinite(snapshot.allowedPriceMin)) runtime.allowedPriceMin = snapshot.allowedPriceMin;
      if (Number.isFinite(snapshot.allowedPriceMax)) runtime.allowedPriceMax = snapshot.allowedPriceMax;
      runtime.allowedAt = snapshot.allowedAt ?? null;
      runtime.itemType = snapshot.itemType || runtime.itemType || config.itemType || null;
      runtime.seenAt = snapshot.seenAt;
      runtime.hydratedAt = Date.now();
      runtime.stale = true;
      // Keep any synthetic points (mock mode) while preferring persisted ones.
      const stored = this.store.loadPriceHistory(itemId, { limit: PRICE_HISTORY_LIMIT });
      if (stored.length > 0) {
        const merged = new Map((runtime.history ?? []).map((point) => [point.t, point]));
        for (const point of stored) merged.set(point.t, point);
        runtime.history = [...merged.values()]
          .sort((a, b) => a.t - b.t)
          .slice(-PRICE_HISTORY_LIMIT);
      }
      restored += 1;
    }

    if (restored > 0) {
      this.logger.info(
        "engine",
        `restored ${restored} cached market snapshots from sqlite — waiting for live frames`
      );
    }
    return restored;
  }

  /** Writes every known snapshot once, used on graceful shutdown. */
  flushMarketCache() {
    if (typeof this.store.saveMarketSnapshots !== "function") return;
    const rows = [];
    for (const [itemId, runtime] of this.runtime) {
      const seenAt = runtime.seenAt ?? runtime.lastSeenAt;
      if (seenAt == null) continue;
      // A target removed from the deck must not resurrect a snapshot row (FK).
      if (!this.#configFor(itemId)) continue;
      rows.push({
        itemId,
        lastPrice: runtime.lastPrice,
        lastStock: runtime.lastStock,
        allowedPriceMin: runtime.allowedPriceMin,
        allowedPriceMax: runtime.allowedPriceMax,
        allowedAt: runtime.allowedAt,
        itemType: runtime.itemType,
        seenAt
      });
    }
    if (rows.length === 0) return;
    try {
      this.store.saveMarketSnapshots(rows);
    } catch (error) {
      this.logger.warn("engine", `could not persist market cache: ${error?.message || error}`);
    }
  }

  resetCounters() {
    for (const state of this.runtime.values()) {
      state.orders = 0;
      state.fills = 0;
      state.failures = 0;
      state.lastResult = null;
    }
  }

  runtimeSnapshot() {
    const out = {};
    for (const [itemId, state] of this.runtime.entries()) {
      const config = this.#configFor(itemId);
      out[itemId] = {
        ...state,
        label: labelFor(config, itemId)
      };
    }
    return out;
  }

  pendingSnapshot() {
    return [...this.pending.entries()].map(([orderId, meta]) => ({ orderId, ...meta }));
  }

  #configFor(itemId) {
    if (!itemId) return null;
    const key = String(itemId).toLowerCase();
    return this.store.state.configs.find((config) => config.itemId.toLowerCase() === key) ?? null;
  }

  #runtime(itemId) {
    const key = String(itemId).toLowerCase();
    let state = this.runtime.get(key);
    if (!state) {
      state = {
        itemId: key,
        lastPrice: null,
        lastStock: null,
        lastSeenAt: null,
        allowedPriceMin: null,
        allowedPriceMax: null,
        allowedAt: null,
        lastOrderAt: null,
        lastInRangeAt: null,
        lastLogAt: 0,
        orders: 0,
        fills: 0,
        failures: 0,
        inflight: false,
        failureStreak: 0,
        blockedUntil: 0,
        lastResult: null,
        itemType: null,
        history: [],
        // Market cache bookkeeping (see hydrateMarket/#persistMarket).
        seenAt: null,
        stale: false,
        hydratedAt: null,
        persistedAt: 0,
        pointWriteAt: 0,
        lastPointBucket: null,
        persistErrorAt: 0
      };
      if (this.mock) state.history = this.#seedMockHistory(key);
      this.runtime.set(key, state);
    }
    return state;
  }

  #seedMockHistory(itemId) {
    const config = this.#configFor(itemId);
    const target = config?.targetPrice || 0.1;
    const now = this.clock();
    const bucket = Math.floor(now / 60_000) * 60_000;
    const points = [];
    let price = target * (0.94 + Math.random() * 0.1);

    for (let minutes = 1439; minutes >= 1; minutes -= 1) {
      price = Math.max(0.000001, price * (0.996 + Math.random() * 0.008));
      points.push({ t: bucket - minutes * 60_000, p: roundPrice(price) });
    }
    points.push({ t: bucket, p: roundPrice(target * (0.97 + Math.random() * 0.06)) });
    return points;
  }

  /**
   * Throttled write of the display cache. Frames can arrive many times per
   * second, so the snapshot is refreshed at most once per window (plus
   * immediately when the marketplace band changes), and the current minute's
   * price point at most every PRICE_POINT_WRITE_INTERVAL_MS. A failed write
   * must never break the frame loop.
   */
  #persistMarket(config, runtime, event, price, bucket, now) {
    try {
      const bandChanged = event.allowedPriceMin != null || event.allowedPriceMax != null;
      if (bandChanged || now - runtime.persistedAt >= MARKET_SNAPSHOT_THROTTLE_MS) {
        this.store.saveMarketSnapshot?.({
          itemId: config.itemId,
          lastPrice: runtime.lastPrice,
          lastStock: runtime.lastStock,
          allowedPriceMin: runtime.allowedPriceMin,
          allowedPriceMax: runtime.allowedPriceMax,
          allowedAt: runtime.allowedAt,
          itemType: runtime.itemType,
          seenAt: runtime.seenAt ?? now
        });
        runtime.persistedAt = now;
      }

      if (
        runtime.lastPointBucket !== bucket ||
        now - runtime.pointWriteAt >= PRICE_POINT_WRITE_INTERVAL_MS
      ) {
        this.store.recordPricePoint?.(config.itemId, bucket, price);
        runtime.lastPointBucket = bucket;
        runtime.pointWriteAt = now;
      }
    } catch (error) {
      if (now - runtime.persistErrorAt > 60_000) {
        runtime.persistErrorAt = now;
        this.logger.warn("engine", `market cache write failed: ${error?.message || error}`);
      }
    }
  }

  handleFrame(frame) {
    let event;
    try {
      event = normalizeUpstreamEvent(frame?.raw ?? frame);
    } catch (error) {
      this.logger.warn("engine", `could not normalise frame: ${error?.message || error}`);
      return;
    }
    if (!event) return;

    if (event.kind === "market" && event.itemId) {
      this.#handleMarket(event);
      return;
    }

    if (event.kind === "fill") {
      this.#handleFill(event);
    }
  }

  #handleMarket(event) {
    const config = this.#configFor(event.itemId);
    if (!config) return;
    if (event.currency && event.currency !== "RLT") return;

    // The socket knows the real item type (miner, rack, battery, ...). Use it
    // to correct configs added before item-info could infer the type.
    if (event.itemType && config.itemType !== event.itemType) {
      config.itemType = event.itemType;
      this.store.save();
      this.onItemType(config.itemId, event.itemType);
    }

    const best = event.listings[0];
    if (!best) return;

    const now = this.clock();
    const runtime = this.#runtime(event.itemId);
    runtime.lastPrice = best.unitPrice;
    runtime.lastStock = best.quantity;
    runtime.lastSeenAt = now;
    runtime.seenAt = now;
    runtime.stale = false;
    runtime.itemType = event.itemType || runtime.itemType || config.itemType || null;

    // Cache the marketplace price band per target: frames that omit it must not
    // wipe the last known values, and the UI needs them after a page reload.
    if (event.allowedPriceMin != null) runtime.allowedPriceMin = event.allowedPriceMin;
    if (event.allowedPriceMax != null) runtime.allowedPriceMax = event.allowedPriceMax;
    if (event.allowedPriceMin != null || event.allowedPriceMax != null) runtime.allowedAt = now;

    // The saved target is the ceiling; the marketplace band is display-only.
    const targetPrice = config.targetPrice;

    const bucket = Math.floor(now / 60_000) * 60_000;
    const history = runtime.history || (runtime.history = []);
    const lastPoint = history[history.length - 1];
    if (!lastPoint || lastPoint.t < bucket) {
      history.push({ t: bucket, p: best.unitPrice });
      if (history.length > 1440) history.splice(0, history.length - 1440);
    } else {
      // Keep one point per minute; update it with the latest price in that minute.
      lastPoint.p = best.unitPrice;
    }

    this.#persistMarket(config, runtime, event, best.unitPrice, bucket, now);

    const settings = this.store.state.settings;
    const plan = planOrder({
      config,
      unitPrice: best.unitPrice,
      targetPrice,
      available: best.quantity,
      paused: settings.paused
    });
    const hot = plan.action === "buy";

    this.broadcast("market", {
      itemId: config.itemId,
      label: labelFor(config, config.itemId),
      price: best.unitPrice,
      stock: best.quantity,
      targetPrice,
      desiredQty: config.desiredQty,
      allowedPriceMin: runtime.allowedPriceMin,
      allowedPriceMax: runtime.allowedPriceMax,
      allowedAt: runtime.allowedAt,
      currency: event.currency || "RLT",
      itemType: event.itemType || runtime.itemType || config.itemType || "mutation_component",
      bucket,
      hot,
      at: now
    });

    if (hot) runtime.lastInRangeAt = now;

    this.#logScan(config, best, plan, targetPrice, now);

    if (!hot) return;
    if (settings.paused) return;
    if (runtime.inflight) return;
    if (runtime.blockedUntil > now) return;

    if (now - this.lastOrderAt < settings.delayPerOrder) {
      this.logger.debug("engine", `${labelFor(config)} in range, waiting for cooldown`);
      return;
    }

    if (!this.#takeRateSlot(now, settings.maxOrdersPerMinute)) {
      if (now - runtime.lastLogAt > 10_000) {
        this.logger.warn("engine", `order cap reached (${settings.maxOrdersPerMinute}/min); ${labelFor(config)} skipped`);
        runtime.lastLogAt = now;
      }
      return;
    }

    if (!settings.dryRun && !this.session.getAccessToken()) {
      if (now - this.lastNoTokenWarnAt > 30_000) {
        this.logger.warn("engine", "signal found but no Rollercoin session is linked; open the Link tab");
        this.lastNoTokenWarnAt = now;
      }
      return;
    }

    this.lastOrderAt = now;
    runtime.lastOrderAt = now;
    runtime.orders += 1;

    const meta = {
      itemId: config.itemId,
      itemType: event.itemType || runtime.itemType || config.itemType || "mutation_component",
      label: labelFor(config, config.itemId),
      qty: plan.qty,
      unitPriceRaw: best.priceRaw,
      unitPrice: best.unitPrice,
      targetPrice,
      notional: plan.notional,
      at: now
    };

    if (settings.dryRun) {
      this.#handleDryRun(meta, plan);
      return;
    }

    this.store.state.stats.ordersSent += 1;
    this.store.save();
    runtime.inflight = true;
    runtime.lastResult = { status: "submitted", at: now, qty: plan.qty, price: best.unitPrice };
    this.broadcast("order", { phase: "submitted", ...meta });
    this.logger.info(
      "order",
      `SEND ${meta.label} ×${plan.qty} @ ${best.unitPrice.toFixed(6)} RLT (${plan.notional.toFixed(6)} total)${plan.doubled ? " [HIGH STOCK ×2]" : ""}`
    );

    this.#submit(meta).catch((error) => {
      this.logger.error("order", `unexpected submit failure: ${error?.message || error}`);
      runtime.inflight = false;
    });
  }

  async #submit(meta) {
    const runtime = this.#runtime(meta.itemId);
    try {
      const result = await this.api.purchase({
        itemId: meta.itemId,
        itemType: meta.itemType,
        qty: meta.qty,
        unitPriceRaw: meta.unitPriceRaw
      });

      if (!result.ok || !result.orderId) {
        const reason = result.error || "marketplace rejected the order";
        runtime.failureStreak = (runtime.failureStreak || 0) + 1;
        const backoff = Math.min(60_000, 5_000 * 2 ** Math.min(runtime.failureStreak - 1, 3));
        runtime.blockedUntil = this.clock() + backoff;
        this.logger.warn(
          "order",
          `REJECTED ${meta.label} ×${meta.qty}: ${reason} — backing off ${Math.round(backoff / 1000)}s`
        );
        runtime.lastResult = { status: "rejected", at: this.clock(), reason };
        this.#recordFailure(meta, reason);
        this.broadcast("order", { phase: "failed", ...meta, reason });
        return;
      }

      const orderId = String(result.orderId);
      this.pending.set(orderId, { ...meta, orderId });
      runtime.failureStreak = 0;
      runtime.blockedUntil = 0;
      runtime.lastResult = { status: "accepted", at: this.clock(), orderId, qty: meta.qty };
      this.logger.info("order", `ACCEPTED ${meta.label} ×${meta.qty} (order ${orderId})`);
      this.broadcast("order", { phase: "accepted", ...meta, orderId });
    } finally {
      runtime.inflight = false;
    }
  }

  #handleDryRun(meta, plan) {
    const runtime = this.#runtime(meta.itemId);
    runtime.lastResult = { status: "dry", at: this.clock(), qty: plan.qty, price: meta.unitPrice };
    this.logger.info(
      "order",
      `DRY RUN — would buy ${meta.label} ×${plan.qty} @ ${meta.unitPrice.toFixed(6)} RLT`
    );
    this.broadcast("order", {
      phase: "dry",
      ...meta,
      reason: "dry-run",
      doubled: Boolean(plan.doubled)
    });
  }

  #handleFill(event) {
    const fill = event.fill;
    if (!fill) return;

    const pendingMeta = fill.orderId ? this.pending.get(fill.orderId) : null;
    const itemId = pendingMeta?.itemId || event.itemId || null;
    const config = this.#configFor(itemId);
    if (!pendingMeta && !config) return;

    const now = this.clock();
    const label = pendingMeta?.label || labelFor(config, itemId);
    const targetPrice = pendingMeta?.targetPrice ?? config?.targetPrice ?? null;

    // A duplicate confirmation must not double-count stats or the loot log.
    // The permanent SQLite log is authoritative: the in-memory window only
    // holds the newest MAX_HISTORY entries.
    if (fill.orderId && this.store.hasOrder(fill.orderId)) {
      this.logger.debug("fill", `ignoring duplicate confirmation ${fill.orderId}`);
      return;
    }

    if (fill.status === ORDER_STATUS.FILLED && fill.count > 0 && fill.priceRaw > 0) {
      const totalPrice = fill.priceRaw / 1_000_000;
      const unitPrice = totalPrice / fill.count;
      const reference = targetPrice ?? unitPrice;
      const profit = roundPrice((reference - unitPrice) * fill.count);

      const stats = this.store.state.stats;
      stats.filled += 1;
      stats.bought += fill.count;
      stats.spentRlt = roundPrice(stats.spentRlt + totalPrice);
      stats.profitRlt = roundPrice(stats.profitRlt + profit);

      const entry = {
        id: fill.orderId || randomUUID(),
        orderId: fill.orderId,
        itemId: String(itemId || ""),
        label,
        qty: fill.count,
        unitPrice: roundPrice(unitPrice),
        totalPrice: roundPrice(totalPrice),
        targetPrice: reference,
        profitRlt: profit,
        status: "filled",
        at: now,
        latencyMs: pendingMeta ? now - pendingMeta.at : null,
        note: null,
        message: "-"
      };

      this.store.addHistory(entry);
      if (fill.orderId) this.pending.delete(fill.orderId);

      const runtime = this.#runtime(itemId);
      runtime.fills += 1;
      // `lastStock` is the book at the cheapest ask, not lifetime inventory.
      // A successful fill reduces it immediately; a failed order never does.
      runtime.lastStock = Math.max(0, Number(runtime.lastStock || 0) - fill.count);
      runtime.lastResult = { status: "filled", at: now, qty: fill.count, price: unitPrice, profit };

      this.logger.info(
        "fill",
        `FILLED ${label} ×${fill.count} @ ${unitPrice.toFixed(6)} RLT | profit ${profit >= 0 ? "+" : ""}${profit.toFixed(6)} RLT | ${entry.latencyMs != null ? `${entry.latencyMs}ms` : "n/a"}`
      );

      this.broadcast("order", { phase: "filled", ...entry, stockAfter: runtime.lastStock });
      this.broadcast("stats", stats);
      this.broadcast("report", this.store.reportView());
      return;
    }

    if (fill.status === ORDER_STATUS.FAILED && fill.count === 0 && fill.priceRaw === 0) {
      if (fill.orderId) this.pending.delete(fill.orderId);
      this.logger.warn("fill", `FAILED ${label} — no items were obtained (order ${fill.orderId || "?"})`);
      this.#recordFailure(pendingMeta || { itemId, label, qty: 0, unitPrice: 0, targetPrice }, "no items obtained", fill.orderId);
      this.broadcast("order", {
        phase: "failed",
        itemId,
        label,
        orderId: fill.orderId,
        reason: "no items obtained",
        at: now
      });
      return;
    }

    this.logger.debug("fill", `ignored fill status ${fill.status} for ${label}`);
  }

  #recordFailure(meta, reason, orderId = null) {
    const stats = this.store.state.stats;
    stats.failed += 1;
    const runtime = this.#runtime(meta.itemId);
    runtime.failures = (runtime.failures || 0) + 1;
    const entry = {
      id: orderId || randomUUID(),
      orderId,
      itemId: String(meta.itemId || ""),
      label: meta.label || labelFor(null, meta.itemId),
      qty: Number(meta.qty) || 0,
      unitPrice: Number(meta.unitPrice) || 0,
      totalPrice: 0,
      targetPrice: Number(meta.targetPrice) || 0,
      profitRlt: 0,
      status: "failed",
      at: this.clock(),
      latencyMs: meta.at ? this.clock() - meta.at : null,
      note: String(reason || "failed").slice(0, 160),
      message: String(reason || "failed").slice(0, 160)
    };
    this.store.addHistory(entry);
    this.broadcast("history", entry);
    this.broadcast("stats", stats);
    this.broadcast("report", this.store.reportView());
  }

  #takeRateSlot(now, maxPerMinute) {
    const cutoff = now - 60_000;
    while (this.orderStamps.length > 0 && this.orderStamps[0] < cutoff) this.orderStamps.shift();
    if (this.orderStamps.length >= maxPerMinute) return false;
    this.orderStamps.push(now);
    return true;
  }

  #logScan(config, best, plan, targetPrice, now) {
    const runtime = this.#runtime(config.itemId);
    if (now - runtime.lastLogAt < SCAN_LOG_INTERVAL_MS) return;
    runtime.lastLogAt = now;
    if (plan.action === "buy") {
      this.logger.info(
        "scan",
        `${labelFor(config)} ${best.unitPrice.toFixed(6)} RLT ×${best.quantity} — IN RANGE (target ${targetPrice.toFixed(6)})`
      );
    } else if (best.unitPrice <= targetPrice) {
      this.logger.info(
        "scan",
        `${labelFor(config)} ${best.unitPrice.toFixed(6)} RLT ×${best.quantity} — IN RANGE, waiting (${plan.reason})`
      );
    } else {
      this.logger.info(
        "scan",
        `${labelFor(config)} ${best.unitPrice.toFixed(6)} RLT ×${best.quantity} — above target ${targetPrice.toFixed(6)}`
      );
    }
  }

  #prunePending() {
    const now = this.clock();
    for (const [orderId, meta] of this.pending.entries()) {
      if (now - meta.at > PENDING_TIMEOUT_MS) {
        this.pending.delete(orderId);
        this.logger.warn("order", `no confirmation for ${orderId} after 120s; releasing it`);
      }
    }
  }
}
