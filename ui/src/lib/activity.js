/**
 * Item activity monitor helpers.
 *
 * The worker ranks item ids by how many market frames they produced in a range
 * (`GET /api/item-activity`). Everything the panel needs that is *not* data
 * fetching lives here so it can be unit-tested without React:
 *
 * * the fixed range presets and the local-calendar boundaries they imply;
 * * category options derived from the rows, so the filter only ever offers
 *   categories the table can actually show;
 * * the name fallback chain (resolved label → short id) and the row formatting
 *   the table renders;
 * * the marketplace URL, copied from `format.js` so a link can be built for an
 *   arbitrary row without importing the whole formatter surface.
 */

import { itemTypeLabel, marketplaceUrl } from "./format.js";

/**
 * The ranges the panel offers. These are rolling windows measured back from
 * "now", not calendar days: at 1A (one hour) a calendar boundary would show
 * almost nothing right after midnight, which is exactly when a busy market is
 * most interesting. The worker buckets counts in 5-minute slices, so every one
 * of these divides evenly and no window is rounded to a partial slice.
 */
export const ACTIVITY_RANGES = Object.freeze([
  { key: "1h", label: "1H", ms: 3_600_000 },
  { key: "3h", label: "3H", ms: 3 * 3_600_000 },
  { key: "12h", label: "12H", ms: 12 * 3_600_000 },
  { key: "1d", label: "1D", ms: 86_400_000 }
]);

/** The widest window the API serves, mirrored from `src/constants.rs`. */
export const ACTIVITY_HISTORY_DAYS = 30;

export const DEFAULT_ACTIVITY_RANGE = "1h";

/** Rows per page, mirrored from `ITEM_ACTIVITY_PAGE_SIZE` in `src/constants.rs`. */
export const ACTIVITY_PAGE_SIZE = 25;

/**
 * The window for one preset, as an inclusive `{since, until}` pair.
 *
 * `until` is the current instant, so the newest slice is always included.
 */
export function rangeForPreset(rangeKey, now = Date.now()) {
  const preset = ACTIVITY_RANGES.find((item) => item.key === rangeKey);
  const span = preset ? preset.ms : ACTIVITY_RANGES[0].ms;
  return { since: now - span, until: now };
}

/** True when the range covers more than one local calendar day. */
export function rangeSpansDays(since, until) {
  const from = new Date(since);
  const to = new Date(until);
  if (Number.isNaN(from.getTime()) || Number.isNaN(to.getTime())) return false;
  return from.toDateString() !== to.toDateString();
}

/** Human label for a row's category, e.g. `mutation_component` → "parts". */
export function categoryOf(row) {
  const key = String(row?.itemType || "").trim().toLowerCase();
  return key || "unknown";
}

/** Display label for a category key, reusing the fleet's vocabulary. */
export function categoryLabel(key) {
  if (!key || key === "unknown") return "Uncategorised";
  return itemTypeLabel(key);
}

/**
 * Category options for the filter select.
 *
 * Derived from the rows the worker returned instead of a hard-coded list: the
 * feed's types drift, and offering a category that can never match would look
 * like a broken filter. The counts come along so each option says how much it
 * would show, and the list is ordered by volume.
 */
export function categoryOptions(rows) {
  const counts = new Map();
  for (const row of Array.isArray(rows) ? rows : []) {
    const key = categoryOf(row);
    counts.set(key, (counts.get(key) || 0) + 1);
  }
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([key, count]) => ({ key, count }));
}

/**
 * Filters rows by category and a free-text term.
 *
 * The term matches the resolved product name *and* the raw item id, so an id
 * copied out of a loot row is findable before its name resolves. Matching and
 * trimming mirror the server (`Store::item_activity`); a client filter can only
 * narrow what the server already ranked, never widen it.
 */
export function filterActivityRows(rows, { itemType = "", query = "" } = {}) {
  const type = String(itemType || "").trim().toLowerCase();
  const term = String(query || "").trim().toLowerCase();
  return (Array.isArray(rows) ? rows : []).filter((row) => {
    if (type && categoryOf(row) !== type) return false;
    if (!term) return true;
    return (
      String(row?.label || "").toLowerCase().includes(term) ||
      String(row?.itemId || "").toLowerCase().includes(term)
    );
  });
}

/**
 * Display name for a row.
 *
 * A resolved name is preferred; when the worker could not name the product the
 * market still moved, so the row keeps its short id rather than disappearing.
 * `pending` marks the unresolved case so the table can show that a lookup is
 * still in flight instead of implying the item has no name.
 */
export function rowLabel(row) {
  const label = String(row?.label || "").trim();
  if (label) return { text: label, pending: false };
  const id = String(row?.itemId || "");
  const tail = id.length > 10 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id;
  return { text: `ITEM ${tail.toUpperCase()}`, pending: true };
}

/** Deep link to the product page for one row. */
export function rowMarketplaceUrl(row) {
  return marketplaceUrl(row?.itemType || "mutation_component", row?.itemId || "");
}

/**
 * A row's share of **all frames counted in the range**, as a whole percentage.
 *
 * This is a proportion of the whole, not a proportion of the leader: with 180
 * frames out of 2 176 the number is 8%, not 100%. The reader can see at a
 * glance how much of the market's traffic a product accounts for, and the
 * column sums to roughly 100% down the table.
 *
 * A row with any activity never rounds down to 0 — at a few thousand total
 * frames a handful of frames is a fraction of a percent, and "0%" beside a
 * non-zero count reads as a bug rather than as "small".
 */
export function sharePercent(events, total) {
  const value = Number(events) || 0;
  const whole = Number(total) || 0;
  if (value <= 0 || whole <= 0) return 0;
  return Math.max(1, Math.round((value / whole) * 100));
}

/** Summary line numbers for the panel header. */
export function summarizeActivity(rows) {
  const list = Array.isArray(rows) ? rows : [];
  let total = 0;
  let tracked = 0;
  for (const row of list) {
    total += Number(row?.events) || 0;
    if (row?.tracked) tracked += 1;
  }
  return { rows: list.length, total, tracked, untracked: list.length - tracked };
}

/**
 * The per-type breakdown shown beside the panel title.
 *
 * The worker sends `typeTotals` computed over the whole range before paging, so
 * this is the one number on screen that does not change as the reader moves
 * between pages. `labels` come from the same vocabulary as the Type column.
 */
export function typeBreakdown(typeTotals) {
  return (Array.isArray(typeTotals) ? typeTotals : [])
    .map((total) => ({
      key: String(total?.itemType || "unknown"),
      events: Number(total?.events) || 0,
      items: Number(total?.items) || 0
    }))
    .filter((total) => total.items > 0)
    .sort((a, b) => b.events - a.events || a.key.localeCompare(b.key));
}

/**
 * Whether there is another batch to append, and what offset to ask for.
 *
 * The table accumulates rows like the loot log, so "pagination" is just a
 * cursor: the number of rows already held is the offset of the next request.
 * Clamping here means a filter that shrinks the result set below what is on
 * screen simply stops asking for more instead of requesting an empty page
 * forever.
 */
export function nextBatch({ shown = 0, total = 0 } = {}) {
  const held = Math.max(0, Math.floor(Number(shown) || 0));
  const count = Math.max(0, Math.floor(Number(total) || 0));
  const remaining = Math.max(0, count - held);
  return {
    offset: held,
    remaining,
    hasMore: remaining > 0
  };
}
