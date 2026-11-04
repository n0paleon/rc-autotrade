import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync
} from "node:fs";
import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { join } from "node:path";
import { MAX_HISTORY, MAX_REPORT_PRODUCTS } from "../shared/constants.js";
import { roundPrice } from "../shared/rules.js";
import { validateSettings, validateTargets } from "../shared/validation.js";

const STATE_VERSION = 1;
const DEFAULT_SETTINGS = Object.freeze({
  delayPerOrder: 200,
  maxOrdersPerMinute: 60,
  dryRun: false,
  autoRefresh: true,
  paused: false
});

export function hashToken(token) {
  return createHash("sha256").update(String(token), "utf8").digest("hex");
}

export function previewToken(token) {
  const value = String(token ?? "");
  if (!value) return null;
  if (value.length <= 12) return `${value.slice(0, 2)}••••${value.slice(-2)}`;
  return `${value.slice(0, 5)}••••${value.slice(-4)}`;
}

export function createWorkerToken() {
  return randomBytes(32).toString("base64url");
}

function asFinite(value, fallback = 0) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : fallback;
}

function asSigned(value, fallback = 0) {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function asNullableTime(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : null;
}

function defaultState() {
  const now = Date.now();
  return {
    version: STATE_VERSION,
    createdAt: now,
    updatedAt: now,
    workerTokenHash: null,
    workerTokenPreview: null,
    session: {
      accessToken: "",
      refreshToken: "",
      cookie: "",
      updatedAt: null,
      source: null,
      lastRefreshAt: null,
      refreshCount: 0,
      lastError: null
    },
    configs: [],
    settings: { ...DEFAULT_SETTINGS },
    stats: {
      since: now,
      profitRlt: 0,
      spentRlt: 0,
      bought: 0,
      filled: 0,
      failed: 0,
      ordersSent: 0
    },
    report: {
      since: now,
      items: {}
    },
    history: []
  };
}

function productKey(itemId, label) {
  const id = String(itemId || "").trim().toLowerCase();
  if (id) return id;
  const name = String(label || "").trim().toLowerCase();
  return name ? `label:${name}` : "unknown";
}

function emptyProduct(entry, at) {
  return {
    itemId: String(entry.itemId || ""),
    label: String(entry.label || "").slice(0, 48),
    fills: 0,
    failed: 0,
    qty: 0,
    spentRlt: 0,
    profitRlt: 0,
    firstAt: at,
    lastAt: at
  };
}

function sanitizeReport(raw, fallbackSince) {
  const source = raw && typeof raw === "object" ? raw : {};
  const rawItems = source.items && typeof source.items === "object" ? source.items : {};
  const items = {};

  for (const [key, value] of Object.entries(rawItems)) {
    if (!value || typeof value !== "object") continue;
    const itemId = String(value.itemId || "").trim();
    const label = String(value.label || "").slice(0, 48);
    if (!itemId && !label) continue;
    const at = asFinite(value.lastAt, Date.now());
    const product = {
      itemId,
      label,
      fills: Math.floor(asFinite(value.fills)),
      failed: Math.floor(asFinite(value.failed)),
      qty: Math.floor(asFinite(value.qty)),
      spentRlt: asFinite(value.spentRlt),
      profitRlt: asSigned(value.profitRlt),
      firstAt: asFinite(value.firstAt, at),
      lastAt: at
    };
    if (product.fills === 0 && product.failed === 0) continue;
    items[String(key)] = product;
  }

  const keys = Object.keys(items);
  if (keys.length > MAX_REPORT_PRODUCTS) {
    keys
      .sort((a, b) => items[b].profitRlt - items[a].profitRlt)
      .slice(MAX_REPORT_PRODUCTS)
      .forEach((key) => delete items[key]);
  }

  return {
    since: asFinite(source.since, fallbackSince),
    items
  };
}

function sanitizeHistory(entries) {
  if (!Array.isArray(entries)) return [];
  return entries
    .filter((entry) => entry && typeof entry === "object")
    .map((entry) => {
      const status = entry.status === "failed" ? "failed" : "filled";
      const note = typeof entry.note === "string" ? entry.note.slice(0, 160) : null;
      // Filled entries show "-", failures carry the rejection reason as their message.
      const message =
        typeof entry.message === "string" && entry.message.trim()
          ? entry.message.slice(0, 160)
          : status === "failed"
            ? note || "failed"
            : "-";
      return {
        id: String(entry.id ?? ""),
        orderId: entry.orderId ? String(entry.orderId) : null,
        itemId: String(entry.itemId ?? ""),
        label: String(entry.label ?? "").slice(0, 48),
        qty: Math.max(0, Math.floor(asFinite(entry.qty))),
        unitPrice: asFinite(entry.unitPrice),
        totalPrice: asFinite(entry.totalPrice),
        targetPrice: asFinite(entry.targetPrice),
        profitRlt: asSigned(entry.profitRlt),
        status,
        at: asFinite(entry.at, Date.now()),
        latencyMs: Number.isFinite(Number(entry.latencyMs)) ? Number(entry.latencyMs) : null,
        note,
        message
      };
    })
    .slice(0, MAX_HISTORY);
}

function sanitizeStats(raw, fallback) {
  const source = raw && typeof raw === "object" ? raw : {};
  return {
    since: asFinite(source.since, fallback.since),
    profitRlt: asFinite(source.profitRlt),
    spentRlt: asFinite(source.spentRlt),
    bought: Math.floor(asFinite(source.bought)),
    filled: Math.floor(asFinite(source.filled)),
    failed: Math.floor(asFinite(source.failed)),
    ordersSent: Math.floor(asFinite(source.ordersSent))
  };
}

/**
 * Persistent runtime state, split into two atomic files:
 *   - state.json    : session, targets, settings, stats (no loot history)
 *   - loot-log.json : confirmed/failed fill history
 *
 * Writes are debounced; `flush()` forces both files synchronously for shutdown.
 */
export class Store {
  constructor({ dataDir, logger }) {
    this.dataDir = dataDir;
    this.logger = logger;
    this.stateFile = join(dataDir, "state.json");
    this.historyFile = join(dataDir, "loot-log.json");
    this.state = defaultState();
    this.dirty = false;
    this.timer = null;
    this.writeErrorLogged = false;
    this.historyDirty = false;
    this.historyTimer = null;
    this.historyErrorLogged = false;
  }

  load() {
    mkdirSync(this.dataDir, { recursive: true, mode: 0o700 });

    let parsed = null;
    let hadLegacyHistory = false;

    if (!existsSync(this.stateFile)) {
      this.logger?.info("store", `fresh state created at ${this.stateFile}`);
    } else {
      try {
        const raw = readFileSync(this.stateFile, "utf8");
        parsed = JSON.parse(raw);
        hadLegacyHistory = Object.prototype.hasOwnProperty.call(parsed ?? {}, "history");
      } catch (error) {
        const backup = `${this.stateFile}.corrupt-${Date.now()}`;
        try {
          renameSync(this.stateFile, backup);
        } catch {
          /* ignore */
        }
        this.logger?.error(
          "store",
          `state.json could not be loaded (${error.message}); backup: ${backup}`
        );
        parsed = null;
      }
    }

    if (parsed) this.#hydrate(parsed);
    this.#loadHistory(parsed?.history);
    this.#backfillReportFromHistory();

    // Even when loot-log.json already exists, rewrite an older state.json once
    // so the history array no longer lives there.
    if (hadLegacyHistory) {
      this.dirty = true;
      this.flush();
    }

    return this;
  }

  #loadHistory(legacyEntries) {
    let loaded = null;

    if (existsSync(this.historyFile)) {
      try {
        const parsed = JSON.parse(readFileSync(this.historyFile, "utf8"));
        loaded = sanitizeHistory(Array.isArray(parsed) ? parsed : parsed?.history);
        this.state.history = loaded;
      } catch (error) {
        const backup = `${this.historyFile}.corrupt-${Date.now()}`;
        try {
          renameSync(this.historyFile, backup);
        } catch {
          /* ignore */
        }
        this.logger?.error(
          "store",
          `loot-log.json could not be loaded (${error.message}); backup: ${backup}`
        );
        loaded = null;
      }
    }

    if (loaded === null) {
      const legacy = sanitizeHistory(legacyEntries);
      this.state.history = legacy;
      if (legacy.length > 0) {
        this.logger?.info(
          "store",
          `migrating ${legacy.length} loot entries from state.json to ${this.historyFile}`
        );
        this.historyDirty = true;
        this.flushHistory();
      }
    }
  }

  #hydrate(raw) {
    const base = defaultState();
    const source = raw && typeof raw === "object" ? raw : {};

    const configsResult = validateTargets(Array.isArray(source.configs) ? source.configs : []);
    if (!configsResult.ok) {
      this.logger?.warn(
        "store",
        `dropped invalid saved targets: ${Object.values(configsResult.errors).slice(0, 3).join(" ")}`
      );
    }

    const settingsResult = validateSettings({
      ...DEFAULT_SETTINGS,
      ...(source.settings && typeof source.settings === "object" ? source.settings : {})
    });

    const sessionSource = source.session && typeof source.session === "object" ? source.session : {};
    const stats = sanitizeStats(source.stats, base.stats);

    this.state = {
      ...base,
      createdAt: asFinite(source.createdAt, base.createdAt),
      updatedAt: asFinite(source.updatedAt, base.updatedAt),
      workerTokenHash: typeof source.workerTokenHash === "string" ? source.workerTokenHash : null,
      workerTokenPreview: typeof source.workerTokenPreview === "string" ? source.workerTokenPreview : null,
      session: {
        accessToken: typeof sessionSource.accessToken === "string" ? sessionSource.accessToken : "",
        refreshToken: typeof sessionSource.refreshToken === "string" ? sessionSource.refreshToken : "",
        cookie: typeof sessionSource.cookie === "string" ? sessionSource.cookie : "",
        updatedAt: asNullableTime(sessionSource.updatedAt),
        source: typeof sessionSource.source === "string" ? sessionSource.source.slice(0, 40) : null,
        lastRefreshAt: asNullableTime(sessionSource.lastRefreshAt),
        refreshCount: Math.floor(asFinite(sessionSource.refreshCount)),
        lastError: typeof sessionSource.lastError === "string" ? sessionSource.lastError.slice(0, 200) : null
      },
      configs: configsResult.values,
      settings: settingsResult.ok ? { ...DEFAULT_SETTINGS, ...settingsResult.value } : { ...DEFAULT_SETTINGS },
      stats,
      report: sanitizeReport(source.report, stats.since),
      history: []
    };
  }

  /**
   * Older state files have no report block. Rebuild it from the loot log once so
   * the panel has data immediately after an upgrade instead of starting empty.
   */
  #backfillReportFromHistory() {
    if (this.state.history.length === 0) return;
    if (Object.keys(this.state.report.items).length > 0) return;
    for (const entry of this.state.history) this.#recordProduct(entry);
    this.state.report.since = Math.min(this.state.report.since, this.state.stats.since);
    this.logger?.info(
      "store",
      `rebuilt profit report for ${Object.keys(this.state.report.items).length} products from the loot log`
    );
    this.dirty = true;
    this.flush();
  }

  /**
   * Resolves the worker access key.
   * Returns plaintext only when a new key was generated (so it can be printed once).
   */
  ensureWorkerToken(envToken = "") {
    if (envToken) {
      const hash = hashToken(envToken);
      if (this.state.workerTokenHash !== hash) {
        this.state.workerTokenHash = hash;
        this.state.workerTokenPreview = previewToken(envToken);
        this.save();
      }
      return { plaintext: null, preview: this.state.workerTokenPreview, source: "env" };
    }

    if (this.state.workerTokenHash) {
      return { plaintext: null, preview: this.state.workerTokenPreview, source: "state" };
    }

    const token = createWorkerToken();
    this.state.workerTokenHash = hashToken(token);
    this.state.workerTokenPreview = previewToken(token);
    this.save();
    this.flush();
    return { plaintext: token, preview: this.state.workerTokenPreview, source: "generated" };
  }

  verifyWorkerToken(token) {
    if (!token || !this.state.workerTokenHash) return false;
    const candidate = Buffer.from(hashToken(token), "utf8");
    const expected = Buffer.from(this.state.workerTokenHash, "utf8");
    if (candidate.length !== expected.length) return false;
    try {
      return timingSafeEqual(candidate, expected);
    } catch {
      return false;
    }
  }

  rotateWorkerToken() {
    const token = createWorkerToken();
    this.state.workerTokenHash = hashToken(token);
    this.state.workerTokenPreview = previewToken(token);
    this.save();
    this.flush();
    return token;
  }

  setSession(patch, { source = "ui" } = {}) {
    const session = this.state.session;
    if (patch.clearAccessToken === true) session.accessToken = "";
    if (patch.clearRefreshToken === true) session.refreshToken = "";
    if (patch.clearCookie === true) session.cookie = "";
    if (typeof patch.accessToken === "string") session.accessToken = patch.accessToken;
    if (typeof patch.refreshToken === "string") session.refreshToken = patch.refreshToken;
    if (typeof patch.cookie === "string") session.cookie = patch.cookie;
    session.source = patch.source || source;
    session.updatedAt = Date.now();
    session.lastError = null;
    this.save();
    return this.sessionStatus();
  }

  markRefreshed({ accessToken, refreshToken }) {
    const session = this.state.session;
    if (accessToken) session.accessToken = accessToken;
    if (refreshToken) session.refreshToken = refreshToken;
    session.lastRefreshAt = Date.now();
    session.refreshCount += 1;
    session.lastError = null;
    session.source = "refresh";
    this.save();
  }

  markSessionError(message) {
    this.state.session.lastError = String(message).slice(0, 200);
    this.save();
  }

  sessionStatus() {
    const session = this.state.session;
    return {
      hasAccessToken: Boolean(session.accessToken),
      hasRefreshToken: Boolean(session.refreshToken),
      hasCookie: Boolean(session.cookie),
      accessPreview: previewToken(session.accessToken),
      refreshPreview: previewToken(session.refreshToken),
      updatedAt: session.updatedAt,
      source: session.source,
      lastRefreshAt: session.lastRefreshAt,
      refreshCount: session.refreshCount,
      lastError: session.lastError,
      expiresAt: jwtExpiry(session.accessToken)
    };
  }

  upsertConfig(target, { originalId = null } = {}) {
    const key = String(originalId || target.itemId).toLowerCase();
    const index = this.state.configs.findIndex((config) => config.itemId.toLowerCase() === key);
    const stamped = { ...target, updatedAt: Date.now() };
    if (index >= 0) {
      stamped.createdAt = this.state.configs[index].createdAt ?? Date.now();
      this.state.configs[index] = stamped;
    } else {
      stamped.createdAt = Date.now();
      this.state.configs.push(stamped);
    }
    this.save();
    return stamped;
  }

  removeConfig(itemId) {
    const key = String(itemId).toLowerCase();
    const before = this.state.configs.length;
    this.state.configs = this.state.configs.filter((config) => config.itemId.toLowerCase() !== key);
    if (this.state.configs.length !== before) this.save();
    return before - this.state.configs.length;
  }

  setConfigs(configs) {
    this.state.configs = configs;
    this.save();
  }

  patchSettings(settings) {
    this.state.settings = { ...this.state.settings, ...settings };
    this.save();
  }

  #recordProduct(entry) {
    if (!entry || typeof entry !== "object") return;
    const report = this.state.report;
    const key = productKey(entry.itemId, entry.label);
    const at = asFinite(entry.at, Date.now());
    let product = report.items[key];
    if (!product) {
      product = emptyProduct(entry, at);
      report.items[key] = product;
    }

    if (entry.label) product.label = String(entry.label).slice(0, 48);
    if (entry.itemId) product.itemId = String(entry.itemId);
    product.firstAt = Math.min(product.firstAt, at);
    product.lastAt = Math.max(product.lastAt, at);

    if (entry.status === "failed") {
      product.failed += 1;
    } else {
      product.fills += 1;
      product.qty += Math.max(0, Math.floor(asFinite(entry.qty)));
      product.spentRlt = roundPrice(product.spentRlt + asFinite(entry.totalPrice));
      product.profitRlt = roundPrice(product.profitRlt + asSigned(entry.profitRlt));
    }

    this.#trimReport();
  }

  #trimReport() {
    const items = this.state.report.items;
    const keys = Object.keys(items);
    if (keys.length <= MAX_REPORT_PRODUCTS) return;
    keys.sort((a, b) => items[a].profitRlt - items[b].profitRlt);
    for (const key of keys.slice(0, keys.length - MAX_REPORT_PRODUCTS)) delete items[key];
  }

  /**
   * All-time profit grouped by product. The aggregate is persisted in
   * state.json, so it keeps counting past MAX_HISTORY loot-log rotation.
   */
  reportView() {
    const report = this.state.report;
    const items = Object.values(report.items)
      .map((product) => {
        const completed = product.fills + product.failed;
        return {
          ...product,
          spentRlt: roundPrice(product.spentRlt),
          profitRlt: roundPrice(product.profitRlt),
          avgBuy: product.qty > 0 ? roundPrice(product.spentRlt / product.qty) : 0,
          marginPct:
            product.spentRlt > 0
              ? Math.round((product.profitRlt / product.spentRlt) * 1000) / 10
              : null,
          successPct: completed > 0 ? Math.round((product.fills / completed) * 100) : null
        };
      })
      .sort(
        (a, b) =>
          b.profitRlt - a.profitRlt ||
          b.spentRlt - a.spentRlt ||
          String(a.label).localeCompare(String(b.label))
      );

    const totals = {
      products: items.length,
      fills: 0,
      failed: 0,
      qty: 0,
      spentRlt: 0,
      profitRlt: 0,
      marginPct: null
    };
    for (const item of items) {
      totals.fills += item.fills;
      totals.failed += item.failed;
      totals.qty += item.qty;
      totals.spentRlt += item.spentRlt;
      totals.profitRlt += item.profitRlt;
    }
    totals.spentRlt = roundPrice(totals.spentRlt);
    totals.profitRlt = roundPrice(totals.profitRlt);
    totals.marginPct =
      totals.spentRlt > 0 ? Math.round((totals.profitRlt / totals.spentRlt) * 1000) / 10 : null;

    return { since: report.since, totals, items };
  }

  addHistory(entry) {
    this.state.history.unshift(entry);
    if (this.state.history.length > MAX_HISTORY) this.state.history.length = MAX_HISTORY;
    this.#recordProduct(entry);
    this.saveHistory();
    this.save();
  }

  resetStats() {
    const now = Date.now();
    this.state.stats = {
      since: now,
      profitRlt: 0,
      spentRlt: 0,
      bought: 0,
      filled: 0,
      failed: 0,
      ordersSent: 0
    };
    this.state.report = { since: now, items: {} };
    this.state.history = [];
    this.saveHistory();
    this.save();
    this.flush();
  }

  save() {
    this.dirty = true;
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      if (this.dirty) this.flush();
    }, 120);
    this.timer.unref?.();
  }

  saveHistory() {
    this.historyDirty = true;
    if (this.historyTimer) return;
    this.historyTimer = setTimeout(() => {
      this.historyTimer = null;
      if (this.historyDirty) this.flushHistory();
    }, 120);
    this.historyTimer.unref?.();
  }

  flushHistory() {
    if (!this.historyDirty) return;
    this.historyDirty = false;
    const temp = `${this.historyFile}.tmp`;
    try {
      writeFileSync(
        temp,
        JSON.stringify(
          {
            version: 1,
            updatedAt: Date.now(),
            history: this.state.history
          },
          null,
          2
        ),
        { mode: 0o600 }
      );
      renameSync(temp, this.historyFile);
      try {
        chmodSync(this.historyFile, 0o600);
      } catch {
        /* non-posix filesystems */
      }
      this.historyErrorLogged = false;
    } catch (error) {
      if (!this.historyErrorLogged) {
        this.logger?.error("store", `failed to persist loot log: ${error.message}`);
        this.historyErrorLogged = true;
      }
      this.historyDirty = true;
    }
  }

  flush() {
    this.flushHistory();
    if (!this.dirty) return;
    this.dirty = false;
    this.state.updatedAt = Date.now();
    const { history: _history, ...persisted } = this.state;
    const temp = `${this.stateFile}.tmp`;
    try {
      writeFileSync(temp, JSON.stringify(persisted, null, 2), { mode: 0o600 });
      renameSync(temp, this.stateFile);
      try {
        chmodSync(this.stateFile, 0o600);
      } catch {
        /* non-posix filesystems */
      }
      this.writeErrorLogged = false;
    } catch (error) {
      if (!this.writeErrorLogged) {
        this.logger?.error("store", `failed to persist state: ${error.message}`);
        this.writeErrorLogged = true;
      }
      // Retry on the next tick rather than losing the mutation entirely.
      this.dirty = true;
    }
  }
}

/**
 * Extracts `exp` (ms) from a JWT-shaped token without verifying it. Used only
 * for display and refresh scheduling; Rollercoin remains the authority.
 */
export function jwtExpiry(token) {
  if (typeof token !== "string") return null;
  const parts = token.split(".");
  if (parts.length !== 3) return null;
  try {
    const payload = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8"));
    return Number.isFinite(payload.exp) ? payload.exp * 1000 : null;
  } catch {
    return null;
  }
}
