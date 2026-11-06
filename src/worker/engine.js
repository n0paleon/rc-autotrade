import { randomUUID } from "node:crypto";
import {
  MARKET_SNAPSHOT_THROTTLE_MS,
  ORDER_STATUS,
  PRICE_HISTORY_LIMIT,
  PRICE_POINT_WRITE_INTERVAL_MS
} from "../shared/constants.js";
import { sanitizeRawEvents } from "../shared/raw-events.js";
import { normalizeUpstreamEvent } from "./events.js";
import { planOrder, roundPrice } from "../shared/rules.js";
import { buildTrend, TREND } from "../shared/trend.js";
import { estimateFillPrice } from "../shared/depth.js";

const PENDING_TIMEOUT_MS = 120_000;
const MAX_UNMATCHED_RESULTS = 128;
const SCAN_LOG_INTERVAL_MS = 12_000;
/** Frames can arrive several times per second; the trend only needs a refresh now and then. */
const TREND_REFRESH_MS = 10_000;

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
    this.unmatchedResults = new Map();
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

        const fillPoints = stored
          .filter((point) => Number.isFinite(point.fillPrice))
          .map((point) => ({ t: point.t, p: point.fillPrice }));
        if (fillPoints.length > 0) {
          const mergedFill = new Map(
            (runtime.fillHistory ?? []).map((point) => [point.t, point])
          );
          for (const point of fillPoints) mergedFill.set(point.t, point);
          runtime.fillHistory = [...mergedFill.values()]
            .sort((a, b) => a.t - b.t)
            .slice(-PRICE_HISTORY_LIMIT);

          // Keep the depth diagnostics available before the first live frame,
          // so the restored trend can still say how many units it priced.
          const latest = stored[stored.length - 1];
          if (Number.isFinite(latest.fillPrice)) {
            runtime.lastFill = {
              price: latest.fillPrice,
              filled: Number.isFinite(latest.fillQty) ? latest.fillQty : null,
              requested: config.desiredQty,
              partial: false,
              approx: Boolean(latest.fillApprox),
              levels: null
            };
          }
          runtime.fillQtyFor = config.desiredQty;
        }
      }
      // Restored history is enough to answer the signal before the first live frame.
      this.#computeTrend(config, runtime, this.clock(), { force: true });
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

  /**
   * Drops every per-target live value (price, stock, counters, trend input)
   * after the store was replaced by a backup. Pending purchases stay tracked so
   * their eventual fills can still be matched by order id.
   */
  resetMarketState() {
    this.runtime.clear();
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
      // `fillHistory` is an internal series: the browser only needs the points
      // behind the chart plus the computed trend, and shipping both histories
      // for every target would double the state payload.
      const { fillHistory, ...runtime } = state;
      out[itemId] = {
        ...runtime,
        label: labelFor(config, itemId)
      };
    }
    return out;
  }

  pendingSnapshot() {
    return [...this.pending.entries()].map(([orderId, meta]) => ({ orderId, ...meta }));
  }

  /**
   * Recomputes the signal for one target and returns it. Used after a target is
   * added or edited so the proximity reading follows the new ceiling at once,
   * without waiting for that item's next market frame.
   */
  refreshTrend(itemId) {
    const config = this.#configFor(itemId);
    if (!config) return null;
    const runtime = this.#runtime(config.itemId);
    this.#syncFillQty(config, runtime);
    return this.#computeTrend(config, runtime, this.clock(), { force: true });
  }

  /**
   * `p_fill` is only comparable across points computed for the same order
   * size. Editing `desiredQty` therefore starts a fresh depth series; the
   * top-ask chart history is untouched.
   */
  #syncFillQty(config, runtime) {
    if (runtime.fillQtyFor === config.desiredQty) return;
    runtime.fillHistory = [];
    runtime.fillQtyFor = config.desiredQty;
    runtime.lastFill = null;
  }

  /**
   * Throttled wrapper around the pure model in shared/trend.js.
   *
   * The previous state is forwarded only when the target is unchanged; after an
   * edit the hysteresis band must be re-entered from scratch, otherwise an old
   * "NEAR" label could survive a move that is no longer near anything.
   */
  #computeTrend(config, runtime, now, { force = false } = {}) {
    const targetPrice = Number(config?.targetPrice) || 0;
    if (
      !force &&
      runtime.trend &&
      runtime.trendTarget === targetPrice &&
      now - runtime.trendAt < TREND_REFRESH_MS
    ) {
      return runtime.trend;
    }

    const previousState =
      runtime.trend && runtime.trendTarget === targetPrice ? runtime.trend.state : null;

    // Prefer the depth series once it covers the model's minimum window; before
    // that (fresh install, first 30 minutes) fall back to the top-ask series
    // and label the source so the UI can be honest about it.
    const fillPoints = runtime.fillHistory || [];
    const fillSpan =
      fillPoints.length > 0 ? fillPoints[fillPoints.length - 1].t - fillPoints[0].t : 0;
    const fillFresh =
      fillPoints.length > 0 &&
      now - fillPoints[fillPoints.length - 1].t <= TREND.maxGapMs;
    const useFill =
      fillPoints.length >= TREND.minPoints && fillSpan >= TREND.minSpanMs && fillFresh;
    const series = useFill ? fillPoints : runtime.history || [];

    runtime.trend = buildTrend(series, {
      now,
      targetPrice,
      previousState,
      bookQty: runtime.lastStock,
      minQty: config?.minAcceptableQty,
      source: useFill ? "depth" : "top",
      fill: runtime.lastFill
    });
    runtime.trendAt = now;
    runtime.trendTarget = targetPrice;
    return runtime.trend;
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
        // Depth-weighted fill price (same one-minute buckets as `history`).
        fillHistory: [],
        fillQtyFor: null,
        lastFill: null,
        // Market cache bookkeeping (see hydrateMarket/#persistMarket).
        seenAt: null,
        stale: false,
        hydratedAt: null,
        persistedAt: 0,
        pointWriteAt: 0,
        lastPointBucket: null,
        persistErrorAt: 0,
        // Signal model cache: recomputed on frames, never on every render.
        trend: null,
        trendAt: 0,
        trendTarget: null
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
  #persistMarket(config, runtime, event, price, fill, bucket, now) {
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
        this.store.recordPricePoint?.(config.itemId, bucket, price, fill);
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
    // `frame.at` is stamped by the gateway when the socket message arrives, so
    // the debugger measures from the signal itself, not from the moment the
    // engine happens to run its handler.
    const receivedAt = Number.isFinite(frame?.at) ? frame.at : this.clock();
    let event;
    try {
      event = normalizeUpstreamEvent(frame?.raw ?? frame);
    } catch (error) {
      this.logger.warn("engine", `could not normalise frame: ${error?.message || error}`);
      return;
    }
    if (!event) return;
    event.rawPayload = frame?.raw ?? frame;

    if (event.kind === "market" && event.itemId) {
      this.#handleMarket(event, receivedAt);
      return;
    }

    if (event.kind === "fill") {
      this.#handleFill(event, receivedAt);
    }
  }

  #handleMarket(event, receivedAt = null) {
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
    // Keep the two cheap marks on every tick; the full timing object is only
    // allocated when this frame actually becomes an order.
    const signalAt = Number.isFinite(receivedAt) ? receivedAt : now;
    const handleAt = now;
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

    // The signal runs on the depth-weighted fill price for this target's order
    // size, not on the cheapest ask: a quantity-1 listing must not define the
    // market. The top-ask series stays untouched for the chart.
    this.#syncFillQty(config, runtime);
    const fill = estimateFillPrice({
      listings: event.listings,
      rest: event.rest,
      qty: config.desiredQty
    });
    runtime.lastFill = fill;
    if (fill) {
      const fillHistory = runtime.fillHistory || (runtime.fillHistory = []);
      const lastFillPoint = fillHistory[fillHistory.length - 1];
      if (!lastFillPoint || lastFillPoint.t < bucket) {
        fillHistory.push({ t: bucket, p: fill.price });
        if (fillHistory.length > 1440) fillHistory.splice(0, fillHistory.length - 1440);
      } else {
        lastFillPoint.p = fill.price;
      }
    }

    // Refresh the signal model before broadcasting so the UI and the snapshot
    // always describe the same frame. This never influences planOrder().
    this.#computeTrend(config, runtime, now);

    this.#persistMarket(config, runtime, event, best.unitPrice, fill, bucket, now);

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
      trend: runtime.trend,
      fill: runtime.lastFill,
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
    const timings = {
      signalAt,
      handleAt,
      orderAt: this.clock(),
      signalCmd: event.cmd || null
    };

    const meta = {
      itemId: config.itemId,
      itemType: event.itemType || runtime.itemType || config.itemType || "mutation_component",
      label: labelFor(config, config.itemId),
      qty: plan.qty,
      unitPriceRaw: best.priceRaw,
      unitPrice: best.unitPrice,
      targetPrice,
      notional: plan.notional,
      at: now,
      timings
    };
    // Keep source frames out of the live order broadcast; persist them only
    // on the eventual loot row.
    Object.defineProperty(meta, "rawPriceEvent", {
      configurable: true,
      value: event.rawPayload
    });

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

    timings.dispatchAt = this.clock();
    this.#submit(meta).catch((error) => {
      this.logger.error("order", `unexpected submit failure: ${error?.message || error}`);
      runtime.inflight = false;
    });
  }

  async #submit(meta) {
    const runtime = this.#runtime(meta.itemId);
    const timing = meta.timings || null;
    if (timing && timing.sendAt == null) timing.sendAt = this.clock();
    let historyId = null;
    let dnsFinished = false;
    const publishDnsUpdate = (id) => {
      if (!id || !this.store.updateHistoryTimings?.(id, timing)) return;
      const entry = this.store.state.history.find((row) => row.id === String(id));
      if (entry) this.broadcast("history", entry);
    };
    const persistLateDns = () => {
      dnsFinished = true;
      if (historyId) publishDnsUpdate(historyId);
    };
    const attachDnsHistory = (id) => {
      historyId = id ? String(id) : null;
      if (historyId && dnsFinished) publishDnsUpdate(historyId);
    };
    if (timing) {
      // Keep the callback out of broadcasts and the persisted timing JSON.
      Object.defineProperty(timing, "onDnsComplete", {
        configurable: true,
        value: persistLateDns
      });
    }
    try {
      const result = await this.api.purchase({
        itemId: meta.itemId,
        itemType: meta.itemType,
        qty: meta.qty,
        unitPriceRaw: meta.unitPriceRaw,
        timing
      });
      if (timing && result?.timing && result.timing !== timing) Object.assign(timing, result.timing);

      const settledAt = this.clock();
      if (timing) {
        // Injected/test API clients do not report their own marks; keep the
        // debugger usable by filling the response boundary with the clock.
        timing.responseAt ??= settledAt;
        timing.completeAt ??= settledAt;
      }

      if (!result.ok || !result.orderId) {
        const reason = result.error || "marketplace rejected the order";
        runtime.failureStreak = (runtime.failureStreak || 0) + 1;
        const backoff = Math.min(60_000, 5_000 * 2 ** Math.min(runtime.failureStreak - 1, 3));
        runtime.blockedUntil = this.clock() + backoff;
        if (timing) timing.failedAt = this.clock();
        this.logger.warn(
          "order",
          `REJECTED ${meta.label} ×${meta.qty}: ${reason} — backing off ${Math.round(backoff / 1000)}s`
        );
        runtime.lastResult = { status: "rejected", at: this.clock(), reason };
        attachDnsHistory(this.#recordFailure(meta, reason));
        this.broadcast("order", { phase: "failed", ...meta, reason });
        return;
      }

      if (timing) timing.acceptedAt = this.clock();
      const orderId = String(result.orderId);
      this.pending.set(orderId, { ...meta, orderId, rawPriceEvent: meta.rawPriceEvent });
      attachDnsHistory(orderId);
      runtime.failureStreak = 0;
      runtime.blockedUntil = 0;
      runtime.lastResult = { status: "accepted", at: this.clock(), orderId, qty: meta.qty };
      this.logger.info("order", `ACCEPTED ${meta.label} ×${meta.qty} (order ${orderId})`);
      this.broadcast("order", { phase: "accepted", ...meta, orderId });

      // The upstream result can beat the purchase HTTP response. Hold unmatched
      // frames briefly, then replay one once its order id is registered.
      const earlyResult = this.unmatchedResults.get(orderId);
      if (earlyResult) {
        this.unmatchedResults.delete(orderId);
        this.#handleFill(earlyResult.event, earlyResult.receivedAt);
      }
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

  #handleFill(event, receivedAt = null) {
    const fill = event.fill;
    if (!fill) return;

    const pendingMeta = fill.orderId ? this.pending.get(fill.orderId) : null;
    if (!pendingMeta) {
      if (fill.orderId) {
        this.#bufferUnmatchedResult(fill.orderId, event, receivedAt);
      } else {
        this.logger.debug("fill", "ignoring result without an order id");
      }
      return;
    }

    const itemId = pendingMeta?.itemId || event.itemId || null;
    const config = this.#configFor(itemId);

    const now = this.clock();
    const fillAt = Number.isFinite(receivedAt) ? receivedAt : now;
    const fillTimings = pendingMeta?.timings
      ? { ...pendingMeta.timings, fillAt, fillHandledAt: now }
      : null;
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
        timings: fillTimings,
        rawEvents: sanitizeRawEvents({
          priceEvent: pendingMeta?.rawPriceEvent ?? null,
          resultEvent: event.rawPayload ?? null
        }),
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
      const failureMeta = pendingMeta
        ? {
            ...pendingMeta,
            timings: fillTimings,
            rawEvents: sanitizeRawEvents({
              priceEvent: pendingMeta.rawPriceEvent ?? null,
              resultEvent: event.rawPayload ?? null
            })
          }
        : {
            itemId,
            label,
            qty: 0,
            unitPrice: 0,
            targetPrice,
            timings: null,
            rawEvents: sanitizeRawEvents({ priceEvent: null, resultEvent: event.rawPayload ?? null })
          };
      this.#recordFailure(failureMeta, "no items obtained", fill.orderId);
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
    const now = this.clock();
    const timings = meta.timings ? { ...meta.timings } : null;
    if (timings && timings.failedAt == null) timings.failedAt = now;
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
      at: now,
      latencyMs: meta.at ? now - meta.at : null,
      timings,
      rawEvents: sanitizeRawEvents(meta.rawEvents ?? {
        priceEvent: meta.rawPriceEvent ?? null,
        resultEvent: meta.rawResultEvent ?? null
      }),
      note: String(reason || "failed").slice(0, 160),
      message: String(reason || "failed").slice(0, 160)
    };
    this.store.addHistory(entry);
    this.broadcast("history", entry);
    this.broadcast("stats", stats);
    this.broadcast("report", this.store.reportView());
    return entry.id;
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
    this.#pruneUnmatchedResults(now);
  }

  #bufferUnmatchedResult(orderId, event, receivedAt) {
    const now = this.clock();
    this.#pruneUnmatchedResults(now);

    const rawPayload =
      sanitizeRawEvents({ resultEvent: event.rawPayload ?? null })?.resultEvent ?? null;
    const bufferedEvent = { ...event, rawPayload };

    // Refresh insertion order for duplicate frames and keep this diagnostic
    // buffer bounded if the socket sends unrelated results in a burst.
    this.unmatchedResults.delete(orderId);
    while (this.unmatchedResults.size >= MAX_UNMATCHED_RESULTS) {
      const oldestOrderId = this.unmatchedResults.keys().next().value;
      if (oldestOrderId === undefined) break;
      this.unmatchedResults.delete(oldestOrderId);
      this.logger.debug("fill", `discarding unmatched result ${oldestOrderId} (buffer full)`);
    }
    this.unmatchedResults.set(orderId, {
      event: bufferedEvent,
      receivedAt,
      bufferedAt: now
    });
    this.logger.debug("fill", `unmatched result ${orderId}; waiting briefly for purchase response`);
  }

  #pruneUnmatchedResults(now) {
    for (const [orderId, result] of this.unmatchedResults.entries()) {
      if (now - result.bufferedAt > PENDING_TIMEOUT_MS) {
        this.unmatchedResults.delete(orderId);
        this.logger.debug("fill", `discarding unmatched result ${orderId} after 120s`);
      }
    }
  }
}
