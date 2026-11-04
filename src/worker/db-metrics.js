/**
 * SQLite performance probe.
 *
 * The worker's hot path writes order history, market snapshots and session
 * state to a local SQLite database. When a WAL checkpoint or a slow write
 * blocks the thread, market frames are delayed and the deck feels laggy, so
 * these metrics sit next to CPU/GC in the SYSTEM panel:
 *
 *   files        main db / -wal / -shm sizes on disk
 *   storage      page size, page count, freelist, payload vs free pages
 *   sqlite       journal mode, synchronous level, cache size, WAL autocheckpoint
 *   queries      cumulative count + latency totals, split by read/write/pragma
 *   window       the same aggregates for the last 60s (bounded statement ring)
 *   transactions commit count and wall time
 *   slow         recent statements over the slow threshold with SQL snippet
 *
 * Instrumentation wraps `db.prepare()` and `db.transaction()` so every
 * `run/get/all` call is timed with `performance.now()`. Only SQL text is kept
 * for slow statements — never bound values, which may contain tokens.
 */

import { statSync } from "node:fs";
import { performance } from "node:perf_hooks";

const DEFAULT_HISTORY = 120;
const MAX_HISTORY = 600;
const DEFAULT_BUFFER = 1_000;
const MAX_BUFFER = 5_000;
const SLOW_CAPACITY = 20;
const BYTES_PER_MB = 1024 * 1024;

/** Alert thresholds. Latencies are milliseconds, sizes are bytes. */
export const DB_THRESHOLDS = Object.freeze({
  slowMs: 20,
  p95WarnMs: 25,
  p95CriticalMs: 100,
  walWarnBytes: 64 * BYTES_PER_MB,
  walCriticalBytes: 256 * BYTES_PER_MB
});

const KIND_BY_KEYWORD = new Map([
  ["select", "read"],
  ["with", "read"],
  ["values", "read"],
  ["insert", "write"],
  ["replace", "write"],
  ["update", "write"],
  ["delete", "write"],
  ["upsert", "write"],
  ["begin", "write"],
  ["commit", "write"],
  ["rollback", "write"],
  ["pragma", "pragma"],
  ["create", "schema"],
  ["alter", "schema"],
  ["drop", "schema"]
]);

function round(value, digits = 3) {
  const number = Number(value);
  if (!Number.isFinite(number)) return 0;
  const factor = 10 ** digits;
  return Math.round(number * factor) / factor;
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function fileSize(path) {
  if (!path) return 0;
  try {
    return statSync(path).size;
  } catch {
    return 0;
  }
}

function safeCall(fn, fallback = null) {
  try {
    return fn();
  } catch {
    return fallback;
  }
}

/** Categorise a statement by its first SQL keyword. */
export function statementKind(sql) {
  const match = /^\s*(?:--[^\n]*\n\s*)*([a-z]+)/i.exec(String(sql || ""));
  const keyword = match ? match[1].toLowerCase() : "";
  return KIND_BY_KEYWORD.get(keyword) || "other";
}

/** One-line, size-bounded SQL text for the slow-query list. */
export function sqlSnippet(sql, limit = 120) {
  const text = String(sql || "").replace(/\s+/g, " ").trim();
  return text.length > limit ? `${text.slice(0, limit - 1)}…` : text;
}

function emptyCounters() {
  return {
    statements: 0,
    read: 0,
    write: 0,
    pragma: 0,
    schema: 0,
    other: 0,
    errors: 0,
    busy: 0,
    slow: 0,
    totalMs: 0,
    maxMs: 0,
    lastMs: null,
    lastAt: null
  };
}

export class DbMetrics {
  constructor({
    db = null,
    files = {},
    now = () => Date.now(),
    historyLimit = DEFAULT_HISTORY,
    bufferLimit = DEFAULT_BUFFER,
    slowMs = DB_THRESHOLDS.slowMs,
    windowMs = 60_000
  } = {}) {
    this.db = db;
    this.files = files || {};
    this.now = now;

    const history = Math.floor(Number(historyLimit));
    this.historyLimit = Number.isFinite(history) ? clamp(history, 2, MAX_HISTORY) : DEFAULT_HISTORY;
    const buffer = Math.floor(Number(bufferLimit));
    this.bufferLimit = Number.isFinite(buffer) ? clamp(buffer, 50, MAX_BUFFER) : DEFAULT_BUFFER;
    const slow = Number(slowMs);
    this.slowMs = Number.isFinite(slow) && slow >= 0 ? slow : DB_THRESHOLDS.slowMs;
    const window = Number(windowMs);
    this.windowMs = Number.isFinite(window) && window > 0 ? window : 60_000;

    this.totals = emptyCounters();
    this.transactions = { count: 0, errors: 0, totalMs: 0, maxMs: 0, lastMs: null, lastAt: null };
    this.samples = [];
    this.slowQueries = [];
    this.history = [];
    this.instrumented = false;
    this.sqliteVersion = null;
    this.previousAt = null;
    this.previousStatements = null;
    this.lastView = null;
    this.thresholds = {
      slowMs: this.slowMs,
      p95WarnMs: DB_THRESHOLDS.p95WarnMs,
      p95CriticalMs: DB_THRESHOLDS.p95CriticalMs,
      walWarnBytes: DB_THRESHOLDS.walWarnBytes,
      walCriticalBytes: DB_THRESHOLDS.walCriticalBytes
    };
  }

  /**
   * Wrap the database so every prepared statement and transaction is timed.
   * Must run before the store prepares its hot statements.
   */
  instrument(db = null) {
    if (this.instrumented) return this;
    const target = db || this.db;
    if (!target || typeof target.prepare !== "function") return this;
    this.db = target;

    const originalPrepare = target.prepare.bind(target);
    const version = safeCall(() => originalPrepare("SELECT sqlite_version() AS version").get(), null);
    this.sqliteVersion = version?.version || null;

    target.prepare = (sql) =>
      this.#wrapStatement(originalPrepare(sql), typeof sql === "string" ? sql : "");

    if (typeof target.transaction === "function") {
      const originalTransaction = target.transaction.bind(target);
      target.transaction = (fn) => {
        const transaction = originalTransaction(fn);
        return (...args) => {
          const started = performance.now();
          try {
            const result = transaction(...args);
            this.#recordTransaction(performance.now() - started, null);
            return result;
          } catch (error) {
            this.#recordTransaction(performance.now() - started, error);
            throw error;
          }
        };
      };
    }

    this.instrumented = true;
    return this;
  }

  /** Capture one sample; the returned view is JSON-safe. */
  sample() {
    const at = this.now();
    const elapsed = this.previousAt === null ? 0 : at - this.previousAt;
    const count = this.totals.statements;
    const delta = this.previousStatements === null ? 0 : count - this.previousStatements;
    const rate = elapsed > 0 ? round((delta / elapsed) * 1000, 1) : 0;
    this.previousAt = at;
    this.previousStatements = count;

    const files = {
      db: fileSize(this.files.db),
      wal: fileSize(this.files.wal),
      shm: fileSize(this.files.shm)
    };
    files.total = files.db + files.wal + files.shm;

    const view = {
      instrumented: this.instrumented,
      files,
      storage: this.#storage(files),
      sqlite: this.#sqliteInfo(),
      queries: {
        ...this.totals,
        avgMs: count > 0 ? round(this.totals.totalMs / count, 3) : 0,
        rate
      },
      transactions: { ...this.transactions },
      window: this.#window(at),
      recentSlow: this.slowQueries.slice(-10).reverse().map((item) => ({ ...item })),
      thresholds: { ...this.thresholds }
    };

    this.history.push({
      at,
      qps: rate,
      avgMs: view.window.avgMs,
      p95Ms: view.window.p95Ms,
      dbBytes: files.db,
      walBytes: files.wal
    });
    if (this.history.length > this.historyLimit) {
      this.history.splice(0, this.history.length - this.historyLimit);
    }

    this.lastView = view;
    return this.snapshot();
  }

  snapshot() {
    if (!this.lastView) this.sample();
    const view = this.lastView;
    return {
      ...view,
      files: { ...view.files },
      storage: { ...view.storage },
      sqlite: { ...view.sqlite },
      queries: { ...view.queries },
      transactions: { ...view.transactions },
      window: { ...view.window },
      thresholds: { ...view.thresholds },
      recentSlow: view.recentSlow.map((item) => ({ ...item })),
      history: this.history.map((point) => ({ ...point }))
    };
  }

  reset() {
    this.totals = emptyCounters();
    this.transactions = { count: 0, errors: 0, totalMs: 0, maxMs: 0, lastMs: null, lastAt: null };
    this.samples.length = 0;
    this.slowQueries.length = 0;
    this.history.length = 0;
    this.previousAt = null;
    this.previousStatements = null;
    this.lastView = null;
  }

  #wrapStatement(statement, sql) {
    const kind = statementKind(sql);
    for (const method of ["run", "get", "all"]) {
      const original = statement[method];
      if (typeof original !== "function") continue;
      const bound = original.bind(statement);
      statement[method] = (...args) => {
        const started = performance.now();
        try {
          const result = bound(...args);
          this.#record(kind, performance.now() - started, sql, null);
          return result;
        } catch (error) {
          this.#record(kind, performance.now() - started, sql, error);
          throw error;
        }
      };
    }
    return statement;
  }

  #record(kind, ms, sql, error) {
    const duration = round(ms, 3);
    const at = this.now();
    const busy = Boolean(error && String(error.code || "").includes("SQLITE_BUSY"));
    const slow = duration >= this.slowMs;

    const totals = this.totals;
    totals.statements += 1;
    if (kind === "read") totals.read += 1;
    else if (kind === "write") totals.write += 1;
    else if (kind === "pragma") totals.pragma += 1;
    else if (kind === "schema") totals.schema += 1;
    else totals.other += 1;
    if (error) totals.errors += 1;
    if (busy) totals.busy += 1;
    totals.totalMs = round(totals.totalMs + duration, 3);
    totals.maxMs = Math.max(totals.maxMs, duration);
    totals.lastMs = duration;
    totals.lastAt = at;

    if (slow) {
      totals.slow += 1;
      this.slowQueries.push({
        at,
        kind,
        ms: duration,
        sql: sqlSnippet(sql),
        ...(error ? { error: String(error.code || error.message || "error").slice(0, 80) } : {})
      });
      if (this.slowQueries.length > SLOW_CAPACITY) {
        this.slowQueries.splice(0, this.slowQueries.length - SLOW_CAPACITY);
      }
    }

    this.samples.push({ at, ms: duration, kind, slow, error: Boolean(error), busy });
    if (this.samples.length > this.bufferLimit) {
      this.samples.splice(0, this.samples.length - this.bufferLimit);
    }
  }

  #recordTransaction(ms, error) {
    const duration = round(ms, 3);
    const at = this.now();
    this.transactions.count += 1;
    this.transactions.totalMs = round(this.transactions.totalMs + duration, 3);
    this.transactions.maxMs = Math.max(this.transactions.maxMs, duration);
    this.transactions.lastMs = duration;
    this.transactions.lastAt = at;
    if (error) this.transactions.errors += 1;
  }

  #window(now) {
    const since = now - this.windowMs;
    const samples = this.samples.filter((sample) => sample.at >= since);
    let read = 0;
    let write = 0;
    let pragma = 0;
    let schema = 0;
    let other = 0;
    let errors = 0;
    let busy = 0;
    let slow = 0;
    let totalMs = 0;
    let maxMs = 0;
    const durations = [];
    for (const sample of samples) {
      if (sample.kind === "read") read += 1;
      else if (sample.kind === "write") write += 1;
      else if (sample.kind === "pragma") pragma += 1;
      else if (sample.kind === "schema") schema += 1;
      else other += 1;
      if (sample.error) errors += 1;
      if (sample.busy) busy += 1;
      if (sample.slow) slow += 1;
      totalMs += sample.ms;
      maxMs = Math.max(maxMs, sample.ms);
      durations.push(sample.ms);
    }
    const count = samples.length;
    let p95Ms = 0;
    if (durations.length > 0) {
      durations.sort((a, b) => a - b);
      p95Ms = durations[Math.min(durations.length - 1, Math.floor(durations.length * 0.95))];
    }
    return {
      windowMs: this.windowMs,
      count,
      read,
      write,
      pragma,
      schema,
      other,
      errors,
      busy,
      slow,
      totalMs: round(totalMs, 3),
      avgMs: count > 0 ? round(totalMs / count, 3) : 0,
      maxMs: round(maxMs, 3),
      p95Ms: round(p95Ms, 3)
    };
  }

  #storage(files) {
    const pageSize = Number(this.#pragma("page_size")) || 0;
    const pageCount = Number(this.#pragma("page_count")) || 0;
    const freelistPages = Number(this.#pragma("freelist_count")) || 0;
    const dataBytes = pageSize > 0 && pageCount > 0 ? pageSize * pageCount : files.db;
    const freeBytes = pageSize > 0 && freelistPages > 0 ? pageSize * freelistPages : 0;
    const usedPages = Math.max(0, pageCount - freelistPages);
    return {
      pageSize,
      pageCount,
      freelistPages,
      dataBytes,
      freeBytes,
      utilizationPercent:
        pageCount > 0 ? round((usedPages / pageCount) * 100, 1) : null
    };
  }

  #sqliteInfo() {
    return {
      driver: this.db?.driver ?? null,
      version: this.sqliteVersion,
      journalMode: this.#pragma("journal_mode"),
      synchronous: this.#pragma("synchronous"),
      cacheSize: this.#pragma("cache_size"),
      walAutocheckpoint: this.#pragma("wal_autocheckpoint"),
      busyTimeout: this.#pragma("busy_timeout")
    };
  }

  #pragma(name) {
    if (!this.db || this.db.open === false) return null;
    return safeCall(() => this.db.pragma(name, { simple: true }), null);
  }
}
