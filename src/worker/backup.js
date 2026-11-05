/**
 * Portable backup and restore for the SQLite store.
 *
 * The live database runs in WAL mode and cannot be swapped under a running
 * worker, so a migration backup serialises the logical content — targets,
 * settings, lifetime stats, the per-product profit aggregate and the full loot
 * log — into a versioned JSON document. Restoring that document validates and
 * normalises every field exactly like the HTTP API does, which means a backup
 * written by an older build can be restored by a newer one without manual SQL.
 *
 * A raw `.db` copy is still produced with `VACUUM INTO` for disaster recovery
 * and as the automatic pre-import safety net. That copy is byte-complete, but
 * it has to be restored with the worker stopped.
 */

import { chmodSync, existsSync, mkdirSync } from "node:fs";
import { join, relative, sep } from "node:path";
import {
  APP_NAME,
  APP_VERSION,
  DEFAULT_SETTINGS,
  LIMITS,
  MAX_REPORT_PRODUCTS
} from "../shared/constants.js";
import { roundPrice } from "../shared/rules.js";
import { validateSettings, validateTarget } from "../shared/validation.js";
import { SCHEMA_VERSION } from "./schema.js";

export const BACKUP_KIND = "turu-rc-backup";
export const BACKUP_FORMAT_VERSION = 1;

/** Maximum POST body accepted by `POST /api/backup` (and mirrored in the UI). */
export const BACKUP_MAX_BYTES = 64 * 1024 * 1024;

/** Defensive cap on loot-log rows in one backup; the body limit is usually hit first. */
export const BACKUP_MAX_ORDERS = 250_000;

/** Directory under DATA_DIR that holds raw `.db` snapshots. */
export const BACKUP_DIRNAME = "backups";

const MAX_WARNINGS = 20;

function plainObject(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function finiteNumber(value, fallback = 0) {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function signedNumber(value, fallback = 0) {
  return finiteNumber(value, fallback);
}

function nonNegative(value, fallback = 0) {
  return Math.max(0, finiteNumber(value, fallback));
}

function wholeNumber(value, fallback = 0, max = Number.MAX_SAFE_INTEGER) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.min(max, Math.max(0, Math.trunc(number)));
}

function positiveTime(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? Math.trunc(number) : null;
}

function timeOr(value, fallback) {
  return positiveTime(value) ?? fallback;
}

function text(value, max) {
  return typeof value === "string" ? value.slice(0, max) : "";
}

function optionalText(value, max) {
  return typeof value === "string" ? value.slice(0, max) : null;
}

function productKey(itemId, label) {
  const id = String(itemId || "").trim().toLowerCase();
  if (id) return id;
  const name = String(label || "").trim().toLowerCase();
  return name ? `label:${name}` : "unknown";
}

/** Marks the collection point of a file name without characters Windows rejects. */
function timestamp(now = Date.now()) {
  const date = now instanceof Date ? now : new Date(now);
  const iso = Number.isNaN(date.getTime()) ? new Date().toISOString() : date.toISOString();
  return iso.replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
}

function safePrefix(value) {
  const clean = String(value || "snapshot").replace(/[^a-zA-Z0-9_-]+/g, "-").replace(/^-+|-+$/g, "");
  return clean || "snapshot";
}

function firstError(errors) {
  return Object.values(errors || {})[0] || "invalid value";
}

function createWarnings() {
  const messages = [];
  let suppressed = 0;
  return {
    add(message) {
      if (messages.length < MAX_WARNINGS) messages.push(String(message));
      else suppressed += 1;
    },
    finish() {
      if (suppressed > 0) {
        messages.push(`${suppressed} more warning${suppressed === 1 ? "" : "s"} hidden.`);
      }
      return messages;
    }
  };
}

// ── export ─────────────────────────────────────────────────────────────────

function targetFromRow(row) {
  return {
    itemId: row.item_id,
    label: row.label,
    itemType: row.item_type,
    ...(row.rarity_color ? { rarityColor: row.rarity_color } : {}),
    targetPrice: row.target_price,
    desiredQty: row.desired_qty,
    highStockThreshold: row.high_stock_threshold,
    minAcceptableQty: row.min_acceptable_qty,
    allowPartialOrder: row.allow_partial_order === 1,
    active: row.active === 1,
    createdAt: row.created_at,
    updatedAt: row.updated_at
  };
}

function orderFromRow(row) {
  return {
    id: row.id,
    orderId: row.order_id,
    itemId: row.item_id,
    label: row.label,
    qty: row.qty,
    unitPrice: row.unit_price,
    totalPrice: row.total_price,
    targetPrice: row.target_price,
    profitRlt: row.profit_rlt,
    status: row.status,
    at: row.at,
    latencyMs: row.latency_ms,
    note: row.note,
    message: row.message
  };
}

function productFromRow(row) {
  return {
    productKey: row.product_key,
    itemId: row.item_id,
    label: row.label,
    fills: row.fills,
    failed: row.failed,
    qty: row.qty,
    spentRlt: row.spent_rlt,
    profitRlt: row.profit_rlt,
    firstAt: row.first_at,
    lastAt: row.last_at
  };
}

function settingsFromRow(row) {
  return {
    delayPerOrder: row.delay_per_order,
    maxOrdersPerMinute: row.max_orders_per_minute,
    dryRun: row.dry_run === 1,
    autoRefresh: row.auto_refresh === 1,
    paused: row.paused === 1
  };
}

function statsFromRow(row) {
  return {
    since: row.since,
    profitRlt: row.profit_rlt,
    spentRlt: row.spent_rlt,
    bought: row.bought,
    filled: row.filled,
    failed: row.failed,
    ordersSent: row.orders_sent
  };
}

function sessionFromRow(row) {
  return {
    accessToken: row.access_token ?? "",
    refreshToken: row.refresh_token ?? "",
    cookie: row.cookie ?? "",
    source: row.source ?? null,
    updatedAt: row.updated_at ?? null,
    lastRefreshAt: row.last_refresh_at ?? null,
    refreshCount: row.refresh_count ?? 0,
    lastError: row.last_error ?? null
  };
}

/**
 * Reads every portable row and wraps it in the versioned backup envelope.
 *
 * @param {import("./store.js").Store} store
 * @param {{includeCredentials?: boolean, now?: number}} [options]
 */
export function buildBackup(store, { includeCredentials = false, now = Date.now() } = {}) {
  if (!store?.db?.open) throw new Error("The database is not open.");
  const db = store.db;

  const meta = {};
  for (const row of db.prepare("SELECT key, value FROM meta").all()) meta[row.key] = row.value;

  const targets = db.prepare("SELECT * FROM targets ORDER BY rowid").all().map(targetFromRow);
  const orders = db.prepare("SELECT * FROM orders ORDER BY at ASC, rowid ASC").all().map(orderFromRow);
  const reportProducts = db
    .prepare("SELECT * FROM report_products ORDER BY product_key")
    .all()
    .map(productFromRow);

  const settingsRow = db.prepare("SELECT * FROM settings WHERE id = 1").get();
  const statsRow = db.prepare("SELECT * FROM stats WHERE id = 1").get();
  const stats = statsRow ? statsFromRow(statsRow) : { ...store.state.stats };
  const createdAt = positiveTime(meta.created_at) ?? store.state.createdAt ?? now;
  const reportSince = positiveTime(meta.report_since) ?? stats.since ?? createdAt;

  const payload = {
    kind: BACKUP_KIND,
    formatVersion: BACKUP_FORMAT_VERSION,
    schemaVersion: SCHEMA_VERSION,
    app: { name: APP_NAME, version: APP_VERSION },
    exportedAt: now,
    counts: {
      targets: targets.length,
      orders: orders.length,
      reportProducts: reportProducts.length
    },
    data: {
      createdAt,
      reportSince,
      settings: settingsRow ? settingsFromRow(settingsRow) : { ...DEFAULT_SETTINGS },
      targets,
      stats,
      reportProducts,
      orders
    }
  };

  if (includeCredentials) {
    const sessionRow = db.prepare("SELECT * FROM session WHERE id = 1").get();
    if (sessionRow) payload.credentials = { session: sessionFromRow(sessionRow) };
  }

  return payload;
}

export function backupFilename(now = Date.now()) {
  return `turu-rc-backup-${timestamp(now)}.json`;
}

export function databaseFilename(now = Date.now()) {
  return `turu-rc-db-${timestamp(now)}.db`;
}

// ── validation ─────────────────────────────────────────────────────────────

function normalizeOrder(entry, warnings, index) {
  if (!plainObject(entry)) {
    warnings.add(`Skipped loot entry #${index + 1}: it is not an object.`);
    return null;
  }
  const id = typeof entry.id === "string" ? entry.id.trim() : "";
  if (!id || id.length > 200) {
    warnings.add(`Skipped loot entry #${index + 1}: it has no usable id.`);
    return null;
  }
  const at = positiveTime(entry.at);
  if (!at) {
    warnings.add(`Skipped loot entry ${id}: its timestamp is invalid.`);
    return null;
  }

  const status = entry.status === "failed" ? "failed" : "filled";
  const note = optionalText(entry.note, 160);
  let message = optionalText(entry.message, 160);
  if (!message || !message.trim()) message = status === "failed" ? note || "failed" : "-";

  return {
    id,
    orderId: typeof entry.orderId === "string" && entry.orderId ? entry.orderId.slice(0, 200) : null,
    itemId: text(entry.itemId, 64),
    label: text(entry.label, 48),
    qty: wholeNumber(entry.qty, 0, 1_000_000_000),
    unitPrice: nonNegative(entry.unitPrice),
    totalPrice: nonNegative(entry.totalPrice),
    targetPrice: nonNegative(entry.targetPrice),
    profitRlt: signedNumber(entry.profitRlt),
    status,
    at,
    latencyMs: entry.latencyMs == null ? null : wholeNumber(entry.latencyMs, 0, 86_400_000),
    note,
    message
  };
}

function deriveStats(orders, now) {
  const filled = orders.filter((order) => order.status === "filled");
  const failed = orders.length - filled.length;
  let spentRlt = 0;
  let profitRlt = 0;
  let bought = 0;
  let since = now;
  for (const order of filled) {
    spentRlt += order.totalPrice;
    profitRlt += order.profitRlt;
    bought += order.qty;
  }
  // Avoid Math.min(...orders): a 250k-row log would overflow the argument stack.
  for (const order of orders) {
    if (order.at > 0 && order.at < since) since = order.at;
  }
  return {
    since,
    profitRlt: roundPrice(profitRlt),
    spentRlt: roundPrice(spentRlt),
    bought,
    filled: filled.length,
    failed,
    ordersSent: orders.length
  };
}

function normalizeStats(raw, derived, warnings) {
  if (!plainObject(raw)) {
    warnings.add(
      raw === undefined
        ? "The backup has no stats block; totals were rebuilt from the loot log."
        : "The stats block was invalid; totals were rebuilt from the loot log."
    );
    return derived;
  }
  const has = (key) => Object.prototype.hasOwnProperty.call(raw, key);
  return {
    since: timeOr(raw.since, derived.since),
    profitRlt: has("profitRlt") ? signedNumber(raw.profitRlt, derived.profitRlt) : derived.profitRlt,
    spentRlt: has("spentRlt") ? nonNegative(raw.spentRlt, derived.spentRlt) : derived.spentRlt,
    bought: has("bought") ? wholeNumber(raw.bought, derived.bought, 1_000_000_000) : derived.bought,
    filled: has("filled") ? wholeNumber(raw.filled, derived.filled) : derived.filled,
    failed: has("failed") ? wholeNumber(raw.failed, derived.failed) : derived.failed,
    ordersSent: has("ordersSent") ? wholeNumber(raw.ordersSent, derived.ordersSent) : derived.ordersSent
  };
}

function normalizeProduct(entry, warnings, index, now) {
  if (!plainObject(entry)) {
    warnings.add(`Skipped profit row #${index + 1}: it is not an object.`);
    return null;
  }
  const itemId = text(entry.itemId, 64);
  const label = text(entry.label, 48);
  if (!itemId && !label) {
    warnings.add(`Skipped profit row #${index + 1}: it has neither item id nor label.`);
    return null;
  }
  const key = text(entry.productKey, 120).trim() || productKey(itemId, label);
  const lastAt = timeOr(entry.lastAt, now);
  const firstAt = Math.min(timeOr(entry.firstAt, lastAt), lastAt);
  return {
    productKey: key,
    itemId,
    label,
    fills: wholeNumber(entry.fills, 0),
    failed: wholeNumber(entry.failed, 0),
    qty: wholeNumber(entry.qty, 0),
    spentRlt: roundPrice(nonNegative(entry.spentRlt)),
    profitRlt: roundPrice(signedNumber(entry.profitRlt)),
    firstAt,
    lastAt
  };
}

/** Rebuilds the per-product aggregate from the loot log when the backup lost it. */
function aggregateProducts(orders) {
  const products = new Map();
  for (const order of orders) {
    const key = productKey(order.itemId, order.label);
    let product = products.get(key);
    if (!product) {
      product = {
        productKey: key,
        itemId: order.itemId,
        label: order.label,
        fills: 0,
        failed: 0,
        qty: 0,
        spentRlt: 0,
        profitRlt: 0,
        firstAt: order.at,
        lastAt: order.at
      };
      products.set(key, product);
    }
    if (order.label) product.label = order.label;
    if (order.itemId) product.itemId = order.itemId;
    product.firstAt = Math.min(product.firstAt, order.at);
    product.lastAt = Math.max(product.lastAt, order.at);
    if (order.status === "failed") {
      product.failed += 1;
    } else {
      product.fills += 1;
      product.qty += order.qty;
      product.spentRlt = roundPrice(product.spentRlt + order.totalPrice);
      product.profitRlt = roundPrice(product.profitRlt + order.profitRlt);
    }
  }
  return [...products.values()]
    .sort((a, b) => b.profitRlt - a.profitRlt || b.spentRlt - a.spentRlt)
    .slice(0, MAX_REPORT_PRODUCTS);
}

function normalizeSession(raw) {
  if (!plainObject(raw)) return null;
  const session = {
    accessToken: text(raw.accessToken, 4096),
    refreshToken: text(raw.refreshToken, 4096),
    cookie: text(raw.cookie, 8192),
    source: optionalText(raw.source, 40),
    updatedAt: positiveTime(raw.updatedAt),
    lastRefreshAt: positiveTime(raw.lastRefreshAt),
    refreshCount: wholeNumber(raw.refreshCount, 0, 1_000_000),
    lastError: optionalText(raw.lastError, 200)
  };
  if (!session.accessToken && !session.refreshToken && !session.cookie) return null;
  return session;
}

/**
 * Validates an untrusted backup document.
 *
 * Fatal problems (wrong kind, future format/schema, unreadable structure) are
 * returned as `errors`; salvageable rows are normalised and reported as
 * `warnings` so a partially damaged file never silently loses a whole import.
 *
 * @param {unknown} raw
 * @param {{now?: number}} [options]
 */
export function parseBackup(raw, { now = Date.now() } = {}) {
  const warnings = createWarnings();
  const errors = {};

  if (!plainObject(raw)) {
    return {
      ok: false,
      errors: { file: "The uploaded value is not a JSON object." },
      warnings: [],
      value: null
    };
  }

  if (raw.kind !== BACKUP_KIND) {
    errors.kind = `This is not a ${APP_NAME} backup (expected kind "${BACKUP_KIND}").`;
  }

  const formatVersion = Number(raw.formatVersion);
  if (!Number.isInteger(formatVersion) || formatVersion < 1) {
    errors.formatVersion = "The backup format version is missing or invalid.";
  } else if (formatVersion > BACKUP_FORMAT_VERSION) {
    errors.formatVersion = `Backup format v${formatVersion} is newer than this build supports (v${BACKUP_FORMAT_VERSION}). Update the worker first.`;
  }

  const schemaVersion = Number(raw.schemaVersion);
  if (!Number.isInteger(schemaVersion) || schemaVersion < 1) {
    errors.schemaVersion = "The database schema version is missing or invalid.";
  } else if (schemaVersion > SCHEMA_VERSION) {
    errors.schemaVersion = `The backup was written with schema v${schemaVersion}, but this build only understands up to v${SCHEMA_VERSION}. Upgrade the worker instead of downgrading the data.`;
  }

  if (!plainObject(raw.data)) errors.data = "The backup has no data block.";

  if (Object.keys(errors).length > 0) {
    return { ok: false, errors, warnings: warnings.finish(), value: null };
  }

  const data = raw.data;

  const settingsInput = plainObject(data.settings) ? data.settings : {};
  if (!plainObject(data.settings)) {
    warnings.add("The backup has no settings block; worker defaults were used.");
  }
  const settingsResult = validateSettings({ ...DEFAULT_SETTINGS, ...settingsInput });
  if (!settingsResult.ok) {
    warnings.add("Some settings were invalid; valid values and defaults were used.");
  }
  const settings = { ...DEFAULT_SETTINGS, ...settingsResult.value };

  let rawTargets = Array.isArray(data.targets) ? data.targets : [];
  if (!Array.isArray(data.targets)) {
    warnings.add("The backup has no target list; this worker will start with zero targets.");
  }
  if (rawTargets.length > LIMITS.configsMax) {
    warnings.add(
      `The backup has ${rawTargets.length} targets, more than the ${LIMITS.configsMax} allowed; only the first ${LIMITS.configsMax} were restored.`
    );
    rawTargets = rawTargets.slice(0, LIMITS.configsMax);
  }
  const targets = [];
  const targetIds = new Set();
  rawTargets.forEach((entry, index) => {
    const result = validateTarget(entry);
    if (!result.ok) {
      warnings.add(`Skipped target #${index + 1}: ${firstError(result.errors)}`);
      return;
    }
    const key = result.value.itemId.toLowerCase();
    if (targetIds.has(key)) {
      warnings.add(`Skipped duplicate target ${result.value.itemId}.`);
      return;
    }
    targetIds.add(key);
    targets.push({
      ...result.value,
      createdAt: timeOr(entry.createdAt, now),
      updatedAt: timeOr(entry.updatedAt, now)
    });
  });

  if (Array.isArray(data.orders) && data.orders.length > BACKUP_MAX_ORDERS) {
    return {
      ok: false,
      errors: {
        orders: `The backup contains ${data.orders.length} loot entries, more than the ${BACKUP_MAX_ORDERS} supported per import.`
      },
      warnings: warnings.finish(),
      value: null
    };
  }

  const rawOrders = Array.isArray(data.orders) ? data.orders : [];
  if (!Array.isArray(data.orders)) {
    warnings.add("The backup has no loot log; the permanent log will be empty.");
  }
  const orders = [];
  const orderIds = new Set();
  rawOrders.forEach((entry, index) => {
    const order = normalizeOrder(entry, warnings, index);
    if (!order) return;
    const key = order.id.toLowerCase();
    if (orderIds.has(key)) {
      warnings.add(`Skipped duplicate loot entry ${order.id}.`);
      return;
    }
    orderIds.add(key);
    orders.push(order);
  });

  const derivedStats = deriveStats(orders, now);
  const stats = normalizeStats(data.stats, derivedStats, warnings);

  let reportProducts = [];
  const rawProducts = Array.isArray(data.reportProducts) ? data.reportProducts : null;
  if (!rawProducts) warnings.add("The backup has no profit report block.");
  const productKeys = new Set();
  (rawProducts ?? []).forEach((entry, index) => {
    const product = normalizeProduct(entry, warnings, index, now);
    if (!product) return;
    if (productKeys.has(product.productKey)) {
      warnings.add(`Skipped duplicate profit row ${product.productKey}.`);
      return;
    }
    productKeys.add(product.productKey);
    reportProducts.push(product);
  });

  if (reportProducts.length > MAX_REPORT_PRODUCTS) {
    warnings.add(
      `The backup has ${reportProducts.length} profit rows; only the ${MAX_REPORT_PRODUCTS} most profitable were restored.`
    );
    reportProducts = reportProducts
      .sort((a, b) => b.profitRlt - a.profitRlt)
      .slice(0, MAX_REPORT_PRODUCTS);
  }
  if (reportProducts.length === 0 && orders.length > 0) {
    reportProducts = aggregateProducts(orders);
    warnings.add("The profit report was rebuilt from the loot log.");
  }

  const declared = plainObject(raw.counts) ? raw.counts : null;
  if (declared) {
    const actual = {
      targets: targets.length,
      orders: orders.length,
      reportProducts: reportProducts.length
    };
    for (const [key, value] of Object.entries(actual)) {
      const claimed = Number(declared[key]);
      if (Number.isFinite(claimed) && claimed !== value) {
        warnings.add(
          `The file claims ${claimed} ${key === "orders" ? "loot entries" : key} but contains ${value}; restoring ${value}.`
        );
      }
    }
  }

  let credentials = null;
  if (plainObject(raw.credentials)) {
    const session = normalizeSession(raw.credentials.session);
    if (session) {
      credentials = { session };
      if (session.accessToken && session.accessToken.length < 8) {
        warnings.add("The stored access token looks unusually short; Rollercoin may reject it.");
      }
    } else {
      warnings.add("The credentials block has no usable session; the current session will be kept.");
    }
  }

  const exportedAt = timeOr(raw.exportedAt, now);
  const createdAt = timeOr(data.createdAt, now);

  return {
    ok: true,
    errors: {},
    warnings: warnings.finish(),
    value: {
      formatVersion,
      schemaVersion,
      exportedAt,
      app: plainObject(raw.app)
        ? { name: text(raw.app.name, 64), version: text(raw.app.version, 32) }
        : null,
      createdAt,
      reportSince: timeOr(data.reportSince, stats.since ?? createdAt),
      settings,
      targets,
      stats,
      reportProducts,
      orders,
      credentials
    }
  };
}

// ── restore ────────────────────────────────────────────────────────────────

/**
 * Writes a consistent, self-contained copy of the live database with
 * `VACUUM INTO`. The file is chmod 0600 because it also contains the session.
 */
export function createDatabaseSnapshot(store, { prefix = "snapshot", now = Date.now() } = {}) {
  if (!store?.db?.open) throw new Error("The database is not open.");
  const dir = join(store.dataDir, BACKUP_DIRNAME);
  mkdirSync(dir, { recursive: true, mode: 0o700 });

  const base = `${safePrefix(prefix)}-${timestamp(now)}`;
  let file = join(dir, `${base}.db`);
  for (let index = 2; existsSync(file); index += 1) {
    file = join(dir, `${base}-${index}.db`);
  }

  try {
    // VACUUM INTO is transactional and includes everything already committed
    // in the WAL, unlike copying the .db file while the worker is running.
    store.db.prepare("VACUUM INTO ?").run(file);
  } catch (error) {
    throw new Error(`Could not write the database snapshot: ${error.message}`);
  }

  try {
    chmodSync(file, 0o600);
  } catch {
    /* filesystems such as exFAT do not support POSIX modes */
  }
  return file;
}

function relativePath(base, file) {
  return relative(base, file).split(sep).join("/");
}

const TARGET_UPSERT = `
  INSERT INTO targets
    (item_id, label, item_type, rarity_color, target_price, desired_qty,
     high_stock_threshold, min_acceptable_qty, allow_partial_order,
     active, created_at, updated_at)
  VALUES
    (@item_id, @label, @item_type, @rarity_color, @target_price, @desired_qty,
     @high_stock_threshold, @min_acceptable_qty, @allow_partial_order,
     @active, @created_at, @updated_at)
`;

const ORDER_INSERT = `
  INSERT OR REPLACE INTO orders
    (id, order_id, item_id, label, qty, unit_price, total_price, target_price,
     profit_rlt, status, at, latency_ms, note, message)
  VALUES
    (@id, @order_id, @item_id, @label, @qty, @unit_price, @total_price, @target_price,
     @profit_rlt, @status, @at, @latency_ms, @note, @message)
`;

const PRODUCT_INSERT = `
  INSERT OR REPLACE INTO report_products
    (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt, first_at, last_at)
  VALUES
    (@product_key, @item_id, @label, @fills, @failed, @qty, @spent_rlt, @profit_rlt, @first_at, @last_at)
`;

const SESSION_UPSERT = `
  INSERT INTO session
    (id, access_token, refresh_token, cookie, source, updated_at, last_refresh_at, refresh_count, last_error)
  VALUES
    (1, @access_token, @refresh_token, @cookie, @source, @updated_at, @last_refresh_at, @refresh_count, @last_error)
  ON CONFLICT(id) DO UPDATE SET
    access_token = excluded.access_token,
    refresh_token = excluded.refresh_token,
    cookie = excluded.cookie,
    source = excluded.source,
    updated_at = excluded.updated_at,
    last_refresh_at = excluded.last_refresh_at,
    refresh_count = excluded.refresh_count,
    last_error = excluded.last_error
`;

const SETTINGS_UPSERT = `
  INSERT INTO settings
    (id, delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused)
  VALUES
    (1, @delay_per_order, @max_orders_per_minute, @dry_run, @auto_refresh, @paused)
  ON CONFLICT(id) DO UPDATE SET
    delay_per_order = excluded.delay_per_order,
    max_orders_per_minute = excluded.max_orders_per_minute,
    dry_run = excluded.dry_run,
    auto_refresh = excluded.auto_refresh,
    paused = excluded.paused
`;

const STATS_UPSERT = `
  INSERT INTO stats
    (id, since, profit_rlt, spent_rlt, bought, filled, failed, orders_sent)
  VALUES
    (1, @since, @profit_rlt, @spent_rlt, @bought, @filled, @failed, @orders_sent)
  ON CONFLICT(id) DO UPDATE SET
    since = excluded.since,
    profit_rlt = excluded.profit_rlt,
    spent_rlt = excluded.spent_rlt,
    bought = excluded.bought,
    filled = excluded.filled,
    failed = excluded.failed,
    orders_sent = excluded.orders_sent
`;

/**
 * Replaces this worker's portable data with a validated backup.
 *
 * The whole write happens in one SQLite transaction. A raw `.db` safety
 * snapshot is taken before it starts; if that snapshot cannot be written the
 * import aborts, so there is always a way back.
 *
 * @param {import("./store.js").Store} store
 * @param {ReturnType<typeof parseBackup>["value"]} backup
 * @param {{restoreCredentials?: boolean, snapshot?: boolean, now?: number}} [options]
 */
export function restoreBackup(
  store,
  backup,
  { restoreCredentials = true, snapshot = true, now = Date.now() } = {}
) {
  if (!store?.db?.open) throw new Error("The database is not open.");
  if (!plainObject(backup)) throw new Error("restoreBackup() needs a validated backup value.");

  const safetyFile = snapshot ? createDatabaseSnapshot(store, { prefix: "pre-import", now }) : null;
  const db = store.db;

  const settings = { ...DEFAULT_SETTINGS, ...(backup.settings || {}) };
  const stats = backup.stats || deriveStats([], now);
  const targets = Array.isArray(backup.targets) ? backup.targets : [];
  const orders = Array.isArray(backup.orders) ? backup.orders : [];
  const products = Array.isArray(backup.reportProducts) ? backup.reportProducts : [];
  const session = restoreCredentials ? backup.credentials?.session ?? null : null;

  const statements = {
    clearSnapshots: db.prepare("DELETE FROM market_snapshot"),
    clearPoints: db.prepare("DELETE FROM price_points"),
    clearOrders: db.prepare("DELETE FROM orders"),
    clearProducts: db.prepare("DELETE FROM report_products"),
    clearTargets: db.prepare("DELETE FROM targets"),
    target: db.prepare(TARGET_UPSERT),
    order: db.prepare(ORDER_INSERT),
    product: db.prepare(PRODUCT_INSERT),
    settings: db.prepare(SETTINGS_UPSERT),
    stats: db.prepare(STATS_UPSERT),
    session: db.prepare(SESSION_UPSERT),
    meta: db.prepare(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    )
  };

  db.transaction(() => {
    // Market cache belongs to the pre-import target list: drop it before the
    // target rows so the foreign key never sees dangling snapshots.
    statements.clearSnapshots.run();
    statements.clearPoints.run();
    statements.clearOrders.run();
    statements.clearProducts.run();
    statements.clearTargets.run();

    statements.settings.run({
      delay_per_order: wholeNumber(settings.delayPerOrder, DEFAULT_SETTINGS.delayPerOrder, 60_000),
      max_orders_per_minute: wholeNumber(
        settings.maxOrdersPerMinute,
        DEFAULT_SETTINGS.maxOrdersPerMinute,
        600
      ),
      dry_run: settings.dryRun ? 1 : 0,
      auto_refresh: settings.autoRefresh ? 1 : 0,
      paused: settings.paused ? 1 : 0
    });

    statements.stats.run({
      since: timeOr(stats.since, now),
      profit_rlt: signedNumber(stats.profitRlt),
      spent_rlt: nonNegative(stats.spentRlt),
      bought: wholeNumber(stats.bought),
      filled: wholeNumber(stats.filled),
      failed: wholeNumber(stats.failed),
      orders_sent: wholeNumber(stats.ordersSent)
    });

    for (const target of targets) {
      statements.target.run({
        item_id: target.itemId,
        label: target.label ?? "",
        item_type: target.itemType ?? "mutation_component",
        rarity_color: target.rarityColor ?? null,
        target_price: Number(target.targetPrice),
        desired_qty: Math.trunc(Number(target.desiredQty)),
        high_stock_threshold: Math.trunc(Number(target.highStockThreshold)),
        min_acceptable_qty: Math.trunc(Number(target.minAcceptableQty)),
        allow_partial_order: target.allowPartialOrder === false ? 0 : 1,
        active: target.active === false ? 0 : 1,
        created_at: timeOr(target.createdAt, now),
        updated_at: timeOr(target.updatedAt, now)
      });
    }

    for (const order of orders) {
      statements.order.run({
        id: order.id,
        order_id: order.orderId ?? null,
        item_id: order.itemId ?? "",
        label: order.label ?? "",
        qty: wholeNumber(order.qty),
        unit_price: nonNegative(order.unitPrice),
        total_price: nonNegative(order.totalPrice),
        target_price: nonNegative(order.targetPrice),
        profit_rlt: signedNumber(order.profitRlt),
        status: order.status === "failed" ? "failed" : "filled",
        at: timeOr(order.at, now),
        latency_ms: order.latencyMs == null ? null : wholeNumber(order.latencyMs),
        note: order.note ?? null,
        message: order.message ?? null
      });
    }

    for (const product of products) {
      statements.product.run({
        product_key: product.productKey,
        item_id: product.itemId ?? "",
        label: product.label ?? "",
        fills: wholeNumber(product.fills),
        failed: wholeNumber(product.failed),
        qty: wholeNumber(product.qty),
        spent_rlt: nonNegative(product.spentRlt),
        profit_rlt: signedNumber(product.profitRlt),
        first_at: timeOr(product.firstAt, now),
        last_at: timeOr(product.lastAt, now)
      });
    }

    statements.meta.run("created_at", String(timeOr(backup.createdAt, now)));
    statements.meta.run("report_since", String(timeOr(backup.reportSince, stats.since ?? now)));

    if (session) {
      statements.session.run({
        access_token: session.accessToken ?? "",
        refresh_token: session.refreshToken ?? "",
        cookie: session.cookie ?? "",
        source: session.source ?? "backup",
        updated_at: session.updatedAt ?? now,
        last_refresh_at: session.lastRefreshAt ?? null,
        refresh_count: wholeNumber(session.refreshCount),
        last_error: session.lastError ?? null
      });
    }
  })();

  store.reload();

  return {
    counts: {
      targets: targets.length,
      orders: orders.length,
      reportProducts: products.length
    },
    credentialsRestored: Boolean(session),
    safetyBackup: safetyFile ? relativePath(store.dataDir, safetyFile) : null,
    safetyBackupPath: safetyFile
  };
}
