import { itemTypeOf } from "./format.js";

/**
 * Target-fleet filtering.
 *
 * The fleet panel composes three narrow filters — arm state, item type and
 * free-text search — and this module is the single place that decides how they
 * intersect. Keeping it here (instead of inline in the panel) means the status
 * chips, the visible cards and the tests all read the same rule.
 */

/** A target is "active" when it is armed; missing/false means parked. */
export function targetIsActive(config) {
  return Boolean(config?.active);
}

/** Totals behind the status chips, so each chip can show its own count. */
export function countTargetsByStatus(configs) {
  const list = Array.isArray(configs) ? configs : [];
  let active = 0;
  for (const config of list) {
    if (targetIsActive(config)) active += 1;
  }
  return { all: list.length, active, inactive: list.length - active };
}

/**
 * Filters targets by status (`all` | `active` | `inactive`), item type and a
 * case-insensitive label/itemId query. An unknown status value falls back to
 * `all` so a stale value can never hide the whole fleet; an unknown type still
 * matches nothing, exactly like the plain equality check it replaces.
 */
export function filterTargets(configs, options = {}) {
  const { search = "", typeFilter = "all", statusFilter = "all" } = options;
  const query = String(search ?? "").trim().toLowerCase();
  const wantsActive = statusFilter === "active";
  const wantsInactive = statusFilter === "inactive";

  return (Array.isArray(configs) ? configs : []).filter((config) => {
    const active = targetIsActive(config);
    if (wantsActive && !active) return false;
    if (wantsInactive && active) return false;
    if (typeFilter !== "all" && itemTypeOf(config) !== typeFilter) return false;
    if (!query) return true;
    return (
      String(config?.label || "").toLowerCase().includes(query) ||
      String(config?.itemId || "").toLowerCase().includes(query)
    );
  });
}
