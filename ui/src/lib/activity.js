/**
 * Item activity monitor helpers.
 *
 * The worker ranks item ids by how many market frames they produced in a range
 * (`GET /api/item-activity`). Everything the panel needs that is *not* data
 * fetching lives here so it can be unit-tested without React:
 *
 * * the fixed range presets and the local-calendar boundaries they imply;
 * * the type breakdown, which keeps every known category visible with a zero
 *   count so the header and its filter do not change shape with the data;
 * * the name fallback chain (resolved label → short id) and the row formatting
 *   the table renders;
 * * the marketplace URL, copied from `format.js` so a link can be built for an
 *   arbitrary row without importing the whole formatter surface.
 */

import { ITEM_TYPE_ORDER, itemTypeLabel, marketplaceUrl } from "./format.js";

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
 * Both the menu and the header use the same range-wide type totals, which are
 * computed before the filters: every known category stays in the list with its
 * whole-range product count, even when the count is zero. That keeps the filter
 * usable while it is narrowing the table — picking "miners" must not remove
 * "racks" from the menu and force a trip back through "all".
 */
export function categoryOptions(typeTotals) {
  return typeBreakdown(typeTotals).map((total) => ({
    key: total.key,
    count: total.items
  }));
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
 * The per-type breakdown shown beside the panel title and in its category menu.
 *
 * The worker sends `typeTotals` computed over the whole range before any
 * category or search filter, so these numbers stay put while the table is
 * narrowed. Every known type is rendered even when the range counted nothing
 * for it: a zero badge is information, while a badge that disappears makes the
 * header jump around. Types the feed has introduced since this build are kept
 * after the known list instead of being dropped.
 */
export function typeBreakdown(typeTotals) {
  const totals = new Map();
  for (const total of Array.isArray(typeTotals) ? typeTotals : []) {
    const key = String(total?.itemType || "").trim().toLowerCase() || "unknown";
    const entry = totals.get(key) || { key, events: 0, items: 0 };
    entry.events += Number(total?.events) || 0;
    entry.items += Number(total?.items) || 0;
    totals.set(key, entry);
  }
  const known = ITEM_TYPE_ORDER.map(
    (key) => totals.get(key) || { key, events: 0, items: 0 }
  );
  const extra = [...totals.keys()]
    .filter((key) => !ITEM_TYPE_ORDER.includes(key))
    .sort((a, b) => a.localeCompare(b))
    .map((key) => totals.get(key));
  return [...known, ...extra];
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
