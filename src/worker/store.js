import { existsSync, mkdirSync, readFileSync, renameSync } from "node:fs";
import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { join } from "node:path";
import {
  LOOT_LOG_MAX_PAGE_SIZE,
  LOOT_LOG_PAGE_SIZE,
  MAX_HISTORY,
  MAX_REPORT_PRODUCTS,
  PRICE_HISTORY_LIMIT,
  PRICE_RETENTION_DAYS
} from "../shared/constants.js";
import { roundPrice } from "../shared/rules.js";
import { validateSettings, validateTargets } from "../shared/validation.js";
import { closeDatabase, openDatabase } from "./db.js";

const STATE_VERSION = 1;
const PRUNE_INTERVAL_MS = 6 * 60 * 60 * 1000;
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

/**
 * Escapes `%`, `_` and `\` so a search term is matched literally inside a
 * SQLite LIKE pattern (`... LIKE ? ESCAPE '\'`).
 */
function escapeLike(value) {
  return String(value).replace(/[\\%_]/g, (match) => `\\${match}`);
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

function sanitizeHistory(entries, limit = MAX_HISTORY) {
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
    .slice(0, limit);
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
 * Persistent runtime state backed by SQLite (`data/rollercoin.db`).
 *
 * Kept API-compatible with the old JSON store: consumers still read
 * `store.state.*` and call `save()`/`flush()`, but writes now touch individual
 * rows inside a transaction instead of rewriting the whole document. The same
 * database also holds the market cache (`market_snapshot` + `price_points`)
 * so a restart can show the last known prices while waiting for fresh frames.
 *
 * Legacy `state.json` / `loot-log.json` files are imported once on first boot
 * and then renamed to `*.migrated-<timestamp>`; nothing is deleted.
 */
export class Store {
  constructor({ dataDir, logger }) {
    this.dataDir = dataDir;
    this.logger = logger;
    this.stateFile = join(dataDir, "state.json");
    this.historyFile = join(dataDir, "loot-log.json");
    this.dbFile = join(dataDir, "rollercoin.db");
    this.db = null;
    this.stmts = null;
    this.state = defaultState();
    this.dirty = false;
    this.timer = null;
    this.writeErrorLogged = false;
    this.pruneTimer = null;
    this.migrated = false;
  }

  load({ dbMetrics = null } = {}) {
    mkdirSync(this.dataDir, { recursive: true, mode: 0o700 });
    this.db = openDatabase({ dataDir: this.dataDir, logger: this.logger });
    // Instrument the handle before the hot prepared statements exist so each
    // run/get/all call is timed by the SYSTEM panel.
    dbMetrics?.instrument(this.db);
    this.stmts = this.#prepareStatements();

    const bootstrapped = this.stmts.metaGet.get("bootstrapped")?.value === "1";
    const imported = bootstrapped ? "skipped" : this.#importLegacyJson();
    this.#ensureRows();
    // A failed import keeps the JSON files untouched and retries on the next
    // boot instead of silently starting from an empty database.
    if (imported !== "failed") this.#setMeta("bootstrapped", "1");

    this.#hydrateFromDb();
    this.#backfillReportIfEmpty();
    this.#prunePriceHistory();
    this.#schedulePrune();

    return this;
  }

  // ── schema plumbing ──────────────────────────────────────────────────────

  #prepareStatements() {
    const db = this.db;
    return {
      metaAll: db.prepare("SELECT key, value FROM meta"),
      metaGet: db.prepare("SELECT value FROM meta WHERE key = ?"),
      metaSet: db.prepare(
        "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value"
      ),
      metaDelete: db.prepare("DELETE FROM meta WHERE key = ?"),

      sessionGet: db.prepare("SELECT * FROM session WHERE id = 1"),
      sessionUpsert: db.prepare(`
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
      `),

      settingsGet: db.prepare("SELECT * FROM settings WHERE id = 1"),
      settingsInit: db.prepare(`
        INSERT OR IGNORE INTO settings
          (id, delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused)
        VALUES (1, @delay_per_order, @max_orders_per_minute, @dry_run, @auto_refresh, @paused)
      `),
      settingsUpsert: db.prepare(`
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
      `),

      statsGet: db.prepare("SELECT * FROM stats WHERE id = 1"),
      statsUpsert: db.prepare(`
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
      `),

      targetsAll: db.prepare("SELECT * FROM targets ORDER BY rowid"),
      targetsClear: db.prepare("DELETE FROM targets"),
      targetUpsert: db.prepare(`
        INSERT INTO targets
          (item_id, label, item_type, rarity_color, target_price, desired_qty,
           high_stock_threshold, min_acceptable_qty, allow_partial_order,
           active, created_at, updated_at)
        VALUES
          (@item_id, @label, @item_type, @rarity_color, @target_price, @desired_qty,
           @high_stock_threshold, @min_acceptable_qty, @allow_partial_order,
           @active, @created_at, @updated_at)
        ON CONFLICT(item_id) DO UPDATE SET
          label = excluded.label,
          item_type = excluded.item_type,
          rarity_color = excluded.rarity_color,
          target_price = excluded.target_price,
          desired_qty = excluded.desired_qty,
          high_stock_threshold = excluded.high_stock_threshold,
          min_acceptable_qty = excluded.min_acceptable_qty,
          allow_partial_order = excluded.allow_partial_order,
          active = excluded.active,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at
      `),

      orderInsert: db.prepare(`
        INSERT INTO orders
          (id, order_id, item_id, label, qty, unit_price, total_price, target_price,
           profit_rlt, status, at, latency_ms, note, message)
        VALUES
          (@id, @order_id, @item_id, @label, @qty, @unit_price, @total_price, @target_price,
           @profit_rlt, @status, @at, @latency_ms, @note, @message)
        ON CONFLICT(id) DO UPDATE SET
          order_id = excluded.order_id,
          item_id = excluded.item_id,
          label = excluded.label,
          qty = excluded.qty,
          unit_price = excluded.unit_price,
          total_price = excluded.total_price,
          target_price = excluded.target_price,
          profit_rlt = excluded.profit_rlt,
          status = excluded.status,
          at = excluded.at,
          latency_ms = excluded.latency_ms,
          note = excluded.note,
          message = excluded.message
      `),
      ordersRecent: db.prepare("SELECT * FROM orders ORDER BY at DESC, rowid DESC LIMIT ?"),
      ordersAll: db.prepare("SELECT * FROM orders ORDER BY at ASC, rowid ASC"),
      ordersClear: db.prepare("DELETE FROM orders"),
      orderExists: db.prepare("SELECT 1 AS hit FROM orders WHERE id = ?"),
      // The loot lookup used by GET /api/history. `q` is an escaped LIKE
      // pattern ('' disables the filter) and `status` is '' | 'filled' | 'failed'.
      ordersCount: db.prepare(`
        SELECT COUNT(*) AS n FROM orders
        WHERE (@q = '' OR label LIKE @q ESCAPE '\\' OR item_id LIKE @q ESCAPE '\\')
          AND (@status = '' OR status = @status)
      `),
      ordersPage: db.prepare(`
        SELECT * FROM orders
        WHERE (@q = '' OR label LIKE @q ESCAPE '\\' OR item_id LIKE @q ESCAPE '\\')
          AND (@status = '' OR status = @status)
        ORDER BY at DESC, rowid DESC
        LIMIT @limit OFFSET @offset
      `),
      // Stable forward cursor: rows inserted while the user pages cannot shift
      // the window (unlike OFFSET), so "load older" never skips an entry.
      ordersPageBefore: db.prepare(`
        SELECT * FROM orders
        WHERE (@q = '' OR label LIKE @q ESCAPE '\\' OR item_id LIKE @q ESCAPE '\\')
          AND (@status = '' OR status = @status)
          AND (
            at < @before_at
            OR (at = @before_at AND rowid < (SELECT rowid FROM orders WHERE id = @before_id))
          )
        ORDER BY at DESC, rowid DESC
        LIMIT @limit
      `),

      productsAll: db.prepare("SELECT * FROM report_products"),
      productsClear: db.prepare("DELETE FROM report_products"),
      productUpsert: db.prepare(`
        INSERT INTO report_products
          (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt, first_at, last_at)
        VALUES
          (@product_key, @item_id, @label, @fills, @failed, @qty, @spent_rlt, @profit_rlt, @first_at, @last_at)
        ON CONFLICT(product_key) DO UPDATE SET
          item_id = excluded.item_id,
          label = excluded.label,
          fills = excluded.fills,
          failed = excluded.failed,
          qty = excluded.qty,
          spent_rlt = excluded.spent_rlt,
          profit_rlt = excluded.profit_rlt,
          first_at = excluded.first_at,
          last_at = excluded.last_at
      `),
      productDelete: db.prepare("DELETE FROM report_products WHERE product_key = ?"),

      snapshotsAll: db.prepare("SELECT * FROM market_snapshot"),
      snapshotUpsert: db.prepare(`
        INSERT INTO market_snapshot
          (item_id, last_price, last_stock, allowed_price_min, allowed_price_max, allowed_at, item_type, seen_at)
        VALUES
          (@item_id, @last_price, @last_stock, @allowed_price_min, @allowed_price_max, @allowed_at, @item_type, @seen_at)
        ON CONFLICT(item_id) DO UPDATE SET
          last_price = excluded.last_price,
          last_stock = excluded.last_stock,
          allowed_price_min = excluded.allowed_price_min,
          allowed_price_max = excluded.allowed_price_max,
          allowed_at = excluded.allowed_at,
          item_type = excluded.item_type,
          seen_at = excluded.seen_at
      `),

      pointsRange: db.prepare(`
        SELECT bucket_at, price, fill_price, fill_qty, fill_approx FROM price_points
        WHERE item_id = ? AND bucket_at >= ?
        ORDER BY bucket_at DESC LIMIT ?
      `),
      pointUpsert: db.prepare(`
        INSERT INTO price_points (item_id, bucket_at, price, fill_price, fill_qty, fill_approx)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(item_id, bucket_at) DO UPDATE SET
          price = excluded.price,
          fill_price = COALESCE(excluded.fill_price, price_points.fill_price),
          fill_qty = COALESCE(excluded.fill_qty, price_points.fill_qty),
          fill_approx = CASE
            WHEN excluded.fill_price IS NULL THEN price_points.fill_approx
            ELSE excluded.fill_approx
          END
      `),
      pointsPrune: db.prepare("DELETE FROM price_points WHERE bucket_at < ?")
    };
  }

  #tx(fn) {
    if (this.db.inTransaction) {
      fn();
      return;
    }
    this.db.transaction(fn)();
  }

  #setMeta(key, value) {
    if (value === null || value === undefined) {
      this.stmts.metaDelete.run(key);
      return;
    }
    this.stmts.metaSet.run(key, String(value));
  }

  #getMeta(key) {
    return this.stmts.metaGet.get(key)?.value ?? null;
  }

  #ensureRows() {
    const now = Date.now();
    this.#tx(() => {
      this.db.prepare("INSERT OR IGNORE INTO session (id) VALUES (1)").run();
      this.stmts.settingsInit.run({
        delay_per_order: DEFAULT_SETTINGS.delayPerOrder,
        max_orders_per_minute: DEFAULT_SETTINGS.maxOrdersPerMinute,
        dry_run: 0,
        auto_refresh: 1,
        paused: 0
      });
      this.db.prepare("INSERT OR IGNORE INTO stats (id, since) VALUES (1, ?)").run(now);
      if (!this.#getMeta("created_at")) this.#setMeta("created_at", now);
    });
  }

  // ── legacy JSON import ───────────────────────────────────────────────────

  #importLegacyJson() {
    const hasState = existsSync(this.stateFile);
    const hasHistory = existsSync(this.historyFile);
    if (!hasState && !hasHistory) return "none";

    let parsed = null;
    if (hasState) {
      try {
        parsed = JSON.parse(readFileSync(this.stateFile, "utf8"));
      } catch (error) {
        this.#archive(`${this.stateFile}.corrupt-${Date.now()}`, this.stateFile);
        this.logger?.error(
          "store",
          `state.json could not be read (${error.message}); left as backup and starting empty`
        );
        parsed = null;
      }
    }

    this.#hydrate(parsed && typeof parsed === "object" ? parsed : null);
    const history = this.#readLegacyHistory(parsed?.history, hasHistory);
    // The whole legacy log goes into SQLite; only the newest slice stays in
    // memory, exactly like a freshly hydrated database.
    this.state.history = history.slice(0, MAX_HISTORY);

    // Older files have no report block: rebuild it from the imported loot log.
    if (Object.keys(this.state.report.items).length === 0 && history.length > 0) {
      for (const entry of history) this.#recordProduct(entry, { persist: false });
      this.state.report.since = Math.min(this.state.report.since, this.state.stats.since);
    }

    try {
      this.#tx(() => {
        this.#writeSession();
        this.#writeSettings();
        this.#writeStats();
        this.stmts.targetsClear.run();
        for (const config of this.state.configs) this.#writeTarget(config);
        for (const entry of history) this.#writeOrder(entry);
        this.stmts.productsClear.run();
        for (const [key, product] of Object.entries(this.state.report.items)) {
          this.#writeProduct(product, key);
        }
        this.#setMeta("created_at", this.state.createdAt);
        this.#setMeta("report_since", this.state.report.since);
        this.#setMeta("worker_token_hash", this.state.workerTokenHash);
        this.#setMeta("worker_token_preview", this.state.workerTokenPreview);
      });
    } catch (error) {
      this.logger?.error("store", `legacy JSON import failed: ${error.message}`);
      return "failed";
    }

    this.#archive(this.stateFile, `${this.stateFile}.migrated-${Date.now()}`);
    this.#archive(this.historyFile, `${this.historyFile}.migrated-${Date.now()}`);
    this.migrated = true;
    this.logger?.info(
      "store",
      `imported legacy JSON into ${this.dbFile}` +
        (history.length ? ` (${history.length} loot entries, ${this.state.configs.length} targets)` : "")
    );
    return "ok";
  }

  #archive(from, to) {
    if (!existsSync(from)) return;
    try {
      renameSync(from, to);
    } catch (error) {
      this.logger?.warn("store", `could not archive ${from}: ${error.message}`);
    }
  }

  #readLegacyHistory(legacyEntries, hasHistoryFile) {
    if (hasHistoryFile) {
      try {
        const parsed = JSON.parse(readFileSync(this.historyFile, "utf8"));
        return sanitizeHistory(Array.isArray(parsed) ? parsed : parsed?.history, Number.POSITIVE_INFINITY);
      } catch (error) {
        this.#archive(`${this.historyFile}.corrupt-${Date.now()}`, this.historyFile);
        this.logger?.error(
          "store",
          `loot-log.json could not be read (${error.message}); falling back to state.json history`
        );
      }
    }
    return sanitizeHistory(legacyEntries, Number.POSITIVE_INFINITY);
  }

  // ── hydration ────────────────────────────────────────────────────────────

  #targetsFromDb() {
    return this.stmts.targetsAll.all().map((row) => ({
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
    }));
  }

  #hydrateFromDb() {
    const base = defaultState();
    const meta = {};
    for (const row of this.stmts.metaAll.all()) meta[row.key] = row.value;

    const configsResult = validateTargets(this.#targetsFromDb());
    if (!configsResult.ok) {
      this.logger?.warn(
        "store",
        `dropped invalid saved targets: ${Object.values(configsResult.errors).slice(0, 3).join(" ")}`
      );
    }

    const settingsRow = this.stmts.settingsGet.get();
    const settingsResult = validateSettings({
      ...DEFAULT_SETTINGS,
      ...(settingsRow
        ? {
            delayPerOrder: settingsRow.delay_per_order,
            maxOrdersPerMinute: settingsRow.max_orders_per_minute,
            dryRun: settingsRow.dry_run === 1,
            autoRefresh: settingsRow.auto_refresh === 1,
            paused: settingsRow.paused === 1
          }
        : {})
    });

    const sessionRow = this.stmts.sessionGet.get() ?? {};
    const statsRow = this.stmts.statsGet.get();
    const stats = sanitizeStats(
      statsRow
        ? {
            since: statsRow.since,
            profitRlt: statsRow.profit_rlt,
            spentRlt: statsRow.spent_rlt,
            bought: statsRow.bought,
            filled: statsRow.filled,
            failed: statsRow.failed,
            ordersSent: statsRow.orders_sent
          }
        : null,
      base.stats
    );

    const products = this.stmts.productsAll.all();
    const items = {};
    for (const row of products) {
      items[row.product_key] = {
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

    const reportSince = Number(meta.report_since) || stats.since;

    this.state = {
      ...base,
      createdAt: Number(meta.created_at) || base.createdAt,
      updatedAt: base.updatedAt,
      workerTokenHash: meta.worker_token_hash || null,
      workerTokenPreview: meta.worker_token_preview || null,
      session: {
        accessToken: typeof sessionRow.access_token === "string" ? sessionRow.access_token : "",
        refreshToken: typeof sessionRow.refresh_token === "string" ? sessionRow.refresh_token : "",
        cookie: typeof sessionRow.cookie === "string" ? sessionRow.cookie : "",
        updatedAt: asNullableTime(sessionRow.updated_at),
        source: typeof sessionRow.source === "string" ? sessionRow.source.slice(0, 40) : null,
        lastRefreshAt: asNullableTime(sessionRow.last_refresh_at),
        refreshCount: Math.floor(asFinite(sessionRow.refresh_count)),
        lastError:
          typeof sessionRow.last_error === "string" ? sessionRow.last_error.slice(0, 200) : null
      },
      configs: configsResult.values,
      settings: settingsResult.ok
        ? { ...DEFAULT_SETTINGS, ...settingsResult.value }
        : { ...DEFAULT_SETTINGS },
      stats,
      report: { since: reportSince, items },
      history: this.stmts.ordersRecent.all(MAX_HISTORY).map((row) => this.#orderToEntry(row))
    };
  }

  /**
   * Older databases may have orders without a report aggregate (legacy
   * loot-log import). Rebuild the aggregate once from every stored order so the
   * panel keeps lifetime totals instead of restarting at zero.
   */
  #backfillReportIfEmpty() {
    if (Object.keys(this.state.report.items).length > 0) return;
    const rows = this.stmts.ordersAll.all();
    if (rows.length === 0) return;

    for (const row of rows) this.#recordProduct(this.#orderToEntry(row), { persist: false });
    this.state.report.since = Math.min(this.state.report.since, this.state.stats.since);

    this.#tx(() => {
      this.stmts.productsClear.run();
      for (const product of Object.values(this.state.report.items)) this.#writeProduct(product);
      this.#setMeta("report_since", this.state.report.since);
    });

    this.logger?.info(
      "store",
      `rebuilt profit report for ${Object.keys(this.state.report.items).length} products from ${rows.length} orders`
    );
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
      settings: settingsResult.ok
        ? { ...DEFAULT_SETTINGS, ...settingsResult.value }
        : { ...DEFAULT_SETTINGS },
      stats,
      report: sanitizeReport(source.report, stats.since),
      history: []
    };
  }

  // ── row writers ──────────────────────────────────────────────────────────

  #writeSession() {
    const session = this.state.session;
    this.stmts.sessionUpsert.run({
      access_token: session.accessToken ?? "",
      refresh_token: session.refreshToken ?? "",
      cookie: session.cookie ?? "",
      source: session.source ?? null,
      updated_at: session.updatedAt ?? null,
      last_refresh_at: session.lastRefreshAt ?? null,
      refresh_count: Math.floor(asFinite(session.refreshCount)),
      last_error: session.lastError ?? null
    });
  }

  #writeSettings() {
    const settings = this.state.settings;
    this.stmts.settingsUpsert.run({
      delay_per_order: settings.delayPerOrder,
      max_orders_per_minute: settings.maxOrdersPerMinute,
      dry_run: settings.dryRun ? 1 : 0,
      auto_refresh: settings.autoRefresh ? 1 : 0,
      paused: settings.paused ? 1 : 0
    });
  }

  #writeStats() {
    const stats = this.state.stats;
    this.stmts.statsUpsert.run({
      since: stats.since,
      profit_rlt: stats.profitRlt,
      spent_rlt: stats.spentRlt,
      bought: stats.bought,
      filled: stats.filled,
      failed: stats.failed,
      orders_sent: stats.ordersSent
    });
  }

  #writeTarget(config) {
    const now = Date.now();
    this.stmts.targetUpsert.run({
      item_id: config.itemId,
      label: config.label ?? "",
      item_type: config.itemType ?? "mutation_component",
      rarity_color: config.rarityColor ?? null,
      target_price: config.targetPrice,
      desired_qty: config.desiredQty,
      high_stock_threshold: config.highStockThreshold,
      min_acceptable_qty: config.minAcceptableQty,
      allow_partial_order: config.allowPartialOrder === false ? 0 : 1,
      active: config.active === false ? 0 : 1,
      created_at: config.createdAt ?? now,
      updated_at: config.updatedAt ?? now
    });
  }

  #writeOrder(entry) {
    this.stmts.orderInsert.run({
      id: entry.id,
      order_id: entry.orderId ?? null,
      item_id: entry.itemId ?? "",
      label: entry.label ?? "",
      qty: entry.qty ?? 0,
      unit_price: entry.unitPrice ?? 0,
      total_price: entry.totalPrice ?? 0,
      target_price: entry.targetPrice ?? 0,
      profit_rlt: entry.profitRlt ?? 0,
      status: entry.status === "failed" ? "failed" : "filled",
      at: entry.at ?? Date.now(),
      latency_ms: entry.latencyMs ?? null,
      note: entry.note ?? null,
      message: entry.message ?? null
    });
  }

  #writeProduct(product, key = productKey(product.itemId, product.label)) {
    this.stmts.productUpsert.run({
      product_key: key,
      item_id: product.itemId ?? "",
      label: product.label ?? "",
      fills: product.fills ?? 0,
      failed: product.failed ?? 0,
      qty: product.qty ?? 0,
      spent_rlt: product.spentRlt ?? 0,
      profit_rlt: product.profitRlt ?? 0,
      first_at: product.firstAt ?? Date.now(),
      last_at: product.lastAt ?? Date.now()
    });
  }

  #orderToEntry(row) {
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

  // ── public API (unchanged surface) ───────────────────────────────────────

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
        this.#persistToken();
      }
      return { plaintext: null, preview: this.state.workerTokenPreview, source: "env" };
    }

    if (this.state.workerTokenHash) {
      return { plaintext: null, preview: this.state.workerTokenPreview, source: "state" };
    }

    const token = createWorkerToken();
    this.state.workerTokenHash = hashToken(token);
    this.state.workerTokenPreview = previewToken(token);
    this.#persistToken();
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
    this.#persistToken();
    return token;
  }

  #persistToken() {
    try {
      this.#tx(() => {
        this.#setMeta("worker_token_hash", this.state.workerTokenHash);
        this.#setMeta("worker_token_preview", this.state.workerTokenPreview);
      });
    } catch (error) {
      this.logger?.error("store", `failed to persist worker key: ${error.message}`);
    }
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
    this.#persistSession();
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
    this.#persistSession();
  }

  markSessionError(message) {
    this.state.session.lastError = String(message).slice(0, 200);
    this.#persistSession();
  }

  #persistSession() {
    try {
      this.#tx(() => this.#writeSession());
    } catch (error) {
      this.logger?.error("store", `failed to persist session: ${error.message}`);
    }
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

  #recordProduct(entry, { persist = true } = {}) {
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

    this.#trimReport(persist);
    // The just-updated product may itself have been trimmed off the list.
    if (persist && report.items[key]) this.#writeProduct(report.items[key], key);
    return product;
  }

  #trimReport(persist = true) {
    const items = this.state.report.items;
    const keys = Object.keys(items);
    if (keys.length <= MAX_REPORT_PRODUCTS) return;
    keys.sort((a, b) => items[a].profitRlt - items[b].profitRlt);
    const drop = keys.slice(0, keys.length - MAX_REPORT_PRODUCTS);
    for (const key of drop) {
      delete items[key];
      if (persist) this.stmts.productDelete.run(key);
    }
  }

  /**
   * All-time profit grouped by product. The aggregate is persisted in
   * `report_products`, so it keeps counting past MAX_HISTORY loot-log rotation.
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

  /**
   * Appends one confirmed/failed order to the permanent loot log.
   *
   * The SQLite `orders` table keeps every row forever; `state.history` is only
   * the newest MAX_HISTORY window used to push live updates to the browser.
   */
  addHistory(entry) {
    const normalized = sanitizeHistory([entry])[0];
    if (!normalized) return;

    this.state.history.unshift(normalized);
    if (this.state.history.length > MAX_HISTORY) this.state.history.length = MAX_HISTORY;

    this.#recordProduct(normalized);
    try {
      // Order and stats commit together: a crash cannot leave a fill in the
      // loot log while the session totals still miss it.
      this.#tx(() => {
        this.#writeOrder(normalized);
        this.#writeStats();
      });
    } catch (error) {
      this.logger?.error("store", `failed to persist order ${normalized.id}: ${error.message}`);
    }
    this.save();
  }

  /** True when an order id already exists in the permanent loot log. */
  hasOrder(id) {
    if (!id) return false;
    return Boolean(this.stmts.orderExists.get(String(id)));
  }

  /** Total number of loot-log rows currently stored (all-time, not just the live window). */
  historyCount() {
    return this.stmts.ordersCount.get({ q: "", status: "" }).n;
  }

  /**
   * One page of the permanent loot log, newest first.
   *
   * - `q` filters by product name (label) and item id; it is matched literally.
   * - `status` narrows to `filled` or `failed` (anything else means "all").
   * - `beforeAt` + `beforeId` form a stable cursor for "load older" so entries
   *   arriving during paging cannot shift the window.
   * - `offset` is only used when no cursor is supplied.
   */
  historyPage({ q = "", status = "", limit = LOOT_LOG_PAGE_SIZE, offset = 0, beforeAt = 0, beforeId = "" } = {}) {
    const term = String(q ?? "").trim().slice(0, 64);
    const like = term ? `%${escapeLike(term)}%` : "";
    const statusFilter = status === "filled" || status === "failed" ? status : "";
    const pageSize = Math.min(
      Math.max(Math.trunc(Number(limit)) || LOOT_LOG_PAGE_SIZE, 1),
      LOOT_LOG_MAX_PAGE_SIZE
    );

    const anchorAt = Number(beforeAt);
    const anchorId = String(beforeId ?? "");
    const cursor =
      Number.isFinite(anchorAt) && anchorAt > 0 && anchorId ? { at: anchorAt, id: anchorId } : null;
    const skip = cursor ? 0 : Math.max(Math.trunc(Number(offset)) || 0, 0);

    // Fetch one extra row to learn whether another page exists.
    const rows = cursor
      ? this.stmts.ordersPageBefore.all({
          q: like,
          status: statusFilter,
          before_at: cursor.at,
          before_id: cursor.id,
          limit: pageSize + 1
        })
      : this.stmts.ordersPage.all({
          q: like,
          status: statusFilter,
          limit: pageSize + 1,
          offset: skip
        });

    const total = this.stmts.ordersCount.get({ q: like, status: statusFilter }).n;
    return {
      entries: rows.slice(0, pageSize).map((row) => this.#orderToEntry(row)),
      total,
      limit: pageSize,
      offset: skip,
      hasMore: rows.length > pageSize
    };
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

    this.#tx(() => {
      this.stmts.ordersClear.run();
      this.stmts.productsClear.run();
      this.#writeStats();
      this.#setMeta("report_since", now);
    });
    this.save();
  }

  // ── market cache ─────────────────────────────────────────────────────────

  /** Latest snapshot per item, keyed by lowercase item id. */
  loadMarketSnapshots() {
    const out = {};
    for (const row of this.stmts.snapshotsAll.all()) {
      out[row.item_id] = {
        lastPrice: row.last_price,
        lastStock: row.last_stock,
        allowedPriceMin: row.allowed_price_min,
        allowedPriceMax: row.allowed_price_max,
        allowedAt: row.allowed_at,
        itemType: row.item_type,
        seenAt: row.seen_at
      };
    }
    return out;
  }

  saveMarketSnapshot(snapshot) {
    this.stmts.snapshotUpsert.run({
      item_id: String(snapshot.itemId).toLowerCase(),
      last_price: snapshot.lastPrice ?? null,
      last_stock: snapshot.lastStock ?? null,
      allowed_price_min: snapshot.allowedPriceMin ?? null,
      allowed_price_max: snapshot.allowedPriceMax ?? null,
      allowed_at: snapshot.allowedAt ?? null,
      item_type: snapshot.itemType ?? null,
      seen_at: snapshot.seenAt ?? Date.now()
    });
  }

  /** Batched variant used on shutdown so a restart resumes with fresh data. */
  saveMarketSnapshots(snapshots) {
    if (!Array.isArray(snapshots) || snapshots.length === 0) return;
    this.#tx(() => {
      for (const snapshot of snapshots) this.saveMarketSnapshot(snapshot);
    });
  }

  /** Oldest → newest minute points for one item. */
  loadPriceHistory(itemId, { limit = PRICE_HISTORY_LIMIT, since = 0 } = {}) {
    const rows = this.stmts.pointsRange.all(String(itemId).toLowerCase(), since, limit);
    return rows.reverse().map((row) => ({
      t: row.bucket_at,
      p: row.price,
      fillPrice: row.fill_price,
      fillQty: row.fill_qty,
      fillApprox: Boolean(row.fill_approx)
    }));
  }

  /**
   * Upserts one minute bucket. `fill` is optional: when a frame could not
   * produce a depth estimate, the previously stored fill columns are kept
   * instead of being wiped by a NULL.
   */
  recordPricePoint(itemId, bucketAt, price, fill = null) {
    const fillPrice = Number.isFinite(fill?.price) ? fill.price : null;
    this.stmts.pointUpsert.run(
      String(itemId).toLowerCase(),
      bucketAt,
      price,
      fillPrice,
      fillPrice != null && Number.isFinite(fill?.filled) ? fill.filled : null,
      fill?.approx ? 1 : 0
    );
  }

  #prunePriceHistory() {
    const cutoff = Date.now() - PRICE_RETENTION_DAYS * 24 * 60 * 60 * 1000;
    try {
      const result = this.stmts.pointsPrune.run(cutoff);
      if (result.changes > 0) {
        this.logger?.info(
          "store",
          `pruned ${result.changes} price points older than ${PRICE_RETENTION_DAYS} days`
        );
      }
    } catch (error) {
      this.logger?.warn("store", `price point prune failed: ${error.message}`);
    }
  }

  #schedulePrune() {
    if (this.pruneTimer) return;
    this.pruneTimer = setInterval(() => this.#prunePriceHistory(), PRUNE_INTERVAL_MS);
    this.pruneTimer.unref?.();
  }

  // ── persistence ──────────────────────────────────────────────────────────

  save() {
    this.dirty = true;
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      if (this.dirty) this.flush();
    }, 120);
    this.timer.unref?.();
  }

  /** Compatibility shims: orders are written immediately in addHistory(). */
  saveHistory() {}

  flushHistory() {}

  flush() {
    if (!this.dirty) return;
    this.dirty = false;
    this.state.updatedAt = Date.now();
    try {
      this.#tx(() => {
        this.#writeSettings();
        this.#writeStats();
        const ids = [];
        for (const config of this.state.configs) {
          ids.push(config.itemId);
          this.#writeTarget(config);
        }
        this.#deleteMissingTargets(ids);
      });
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

  #deleteMissingTargets(ids) {
    if (ids.length === 0) {
      this.stmts.targetsClear.run();
      return;
    }
    const placeholders = ids.map(() => "?").join(",");
    this.db.prepare(`DELETE FROM targets WHERE item_id NOT IN (${placeholders})`).run(...ids);
  }

  close() {
    if (this.pruneTimer) clearInterval(this.pruneTimer);
    this.pruneTimer = null;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.flush();
    closeDatabase(this.db);
    this.db = null;
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
