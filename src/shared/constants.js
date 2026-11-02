/**
 * Shared constants used by the worker, the API and the tests.
 * Keep this module dependency-free so it can run anywhere.
 */
export const APP_NAME = "TURU.RC";
export const APP_VERSION = "1.0.0";

/** Rollercoin reports marketplace prices in micro-RLT (1 RLT = 1_000_000). */
export const PRICE_SCALE = 1_000_000;

/** How many confirmed/failed orders we keep in the loot log. */
export const MAX_HISTORY = 200;

/** How many log lines are buffered for clients that connect late. */
export const LOG_BUFFER_SIZE = 250;

/** How many upstream events we keep as a rolling sample for the feed. */
export const EVENT_SAMPLE_SIZE = 60;

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
  historyMax: MAX_HISTORY
});

export const ORDER_STATUS = Object.freeze({
  FILLED: 12,
  FAILED: 2
});
