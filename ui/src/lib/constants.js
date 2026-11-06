/**
 * UI-side constants.
 *
 * The backend is Rust (`src/constants.rs`); the numbers below mirror the
 * defaults and limits it enforces so the deck can render before the first
 * snapshot arrives and pre-validate obvious mistakes. Keep them in sync — the
 * server always re-validates, so a stale value here cannot bypass a limit.
 */

/** Runtime defaults used before the first `/api/state` snapshot arrives. */
export const DEFAULT_SETTINGS = Object.freeze({
  delayPerOrder: 200,
  maxOrdersPerMinute: 60,
  dryRun: false,
  autoRefresh: true,
  paused: false
});

/** How many loot-log rows one `/api/history` page returns. */
export const LOOT_LOG_PAGE_SIZE = 50;

/** Upper bound for the persistent per-product profit report. */
export const MAX_REPORT_PRODUCTS = 500;

/** Mirrors the server-side validation limits (`src/validation.rs`). */
export const LIMITS = Object.freeze({
  targetPriceMax: 1_000_000,
  qtyMax: 10_000,
  stockThresholdMax: 1_000_000,
  delayMin: 50,
  delayMax: 60_000,
  ordersPerMinuteMin: 1,
  ordersPerMinuteMax: 600,
  labelMax: 48,
  configsMax: 100,
  bodyBytes: 256 * 1024,
  historyMax: 200
});
