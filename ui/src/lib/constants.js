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
  mode: "live",
  autoRefresh: true
});

/**
 * The three trading modes. `off` stops deciding (and spends no cooldown or
 * rate-limit budget), `shadow` decides and logs but never submits, `live`
 * submits. This replaces the old independent `paused` + `dryRun` pair.
 */
export const MODES = Object.freeze([
  {
    id: "off",
    label: "Off",
    short: "OFF",
    hint: "Stop deciding. The market feed stays live and no budget is spent."
  },
  {
    id: "shadow",
    label: "Shadow",
    short: "SHADOW",
    hint: "Log every signal it would trade, but never send an order."
  },
  {
    id: "live",
    label: "Live",
    short: "LIVE",
    hint: "Send real orders. Needs a linked Rollercoin session."
  }
]);

/** Normalises a stored/legacy settings object into one of the three modes. */
export function modeOf(settings = {}) {
  if (MODES.some((mode) => mode.id === settings.mode)) return settings.mode;
  if (settings.paused) return "off";
  if (settings.dryRun) return "shadow";
  return "live";
}

/** How many loot-log rows one `/api/history` page returns. */
export const LOOT_LOG_PAGE_SIZE = 50;

/**
 * Control-socket telemetry cadence, mirrored from `TELEMETRY_INTERVAL_SECS` in
 * `src/constants.rs`. The worker pushes `gateway` (which carries `eventCount`),
 * `system` and `latency` together this often.
 */
export const TELEMETRY_PUSH_MS = 3000;

/**
 * A gap this long without a fresh `eventCount` means the feed stalled, so the
 * `/s` readout starts decaying instead of holding a stale rate. Kept a little
 * above the push cadence: the counter updates in steps, and decaying before the
 * next step arrives would make the number flicker.
 */
export const EVENT_RATE_STALE_MS = TELEMETRY_PUSH_MS + 900;

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
