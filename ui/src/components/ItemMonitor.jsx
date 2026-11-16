import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  ACTIVITY_PAGE_SIZE,
  ACTIVITY_RANGES,
  ACTIVITY_SORT_COLUMNS,
  categoryLabel,
  categoryOf,
  categoryOptions,
  DEFAULT_ACTIVITY_RANGE,
  fetchActivityRows,
  nextActivitySort,
  nextBatch,
  priceSpreadPct,
  rangeForPreset,
  rangeSpansDays,
  rowLabel,
  rowMarketplaceUrl,
  sharePercent,
  sortActivityRows,
  summarizeActivity,
  typeBreakdown
} from "../lib/activity.js";
import {
  fixedPrice,
  formatInt,
  formatPercent,
  formatTime,
  itemTypeLabel,
  relTime
} from "../lib/format.js";
import { useCollapsed, useMediaQuery } from "../lib/hooks.js";
import { CollapseToggle } from "./CollapseToggle.jsx";
import { Icon } from "./ModalShell.jsx";
import { copyText } from "./Modals.jsx";

/** How often the table re-reads the worker while the tab is visible. */
const REFRESH_MS = 30_000;
const SEARCH_DEBOUNCE_MS = 250;

/** Column lookup shared by the table headers and the sort controls. */
const SORT_COLUMNS = Object.fromEntries(
  ACTIVITY_SORT_COLUMNS.map((column) => [column.key, column])
);

function dayLabel(ts) {
  return new Date(ts).toLocaleDateString("en-GB", { day: "2-digit", month: "short" });
}

function clockLabel(ts) {
  return new Date(ts).toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false
  });
}

/**
 * "22:10–02:24" or "13 Nov 02:24 – 14 Nov 02:24" caption for the current window.
 *
 * The caption is what tells a reader which wall-clock stretch the numbers
 * cover, so it follows the viewer's local clock rather than the server's. A
 * window that crosses midnight keeps both the date and the time, because
 * "13 Nov – 02:24" would read like an hour range rather than a day.
 */
function rangeCaption(since, until) {
  if (!Number.isFinite(since) || !Number.isFinite(until)) return "—";
  if (!rangeSpansDays(since, until)) {
    return `${clockLabel(since)}–${clockLabel(until)}`;
  }
  return `${dayLabel(since)} ${clockLabel(since)} – ${dayLabel(until)} ${clockLabel(until)}`;
}

function Segmented({ label, options, value, onChange }) {
  return (
    <div className="flex items-center gap-1.5">
      <span className="font-mono text-[11px] uppercase tracking-[0.1em] text-faint max-sm:hidden">
        {label}
      </span>
      <div
        className="flex items-center gap-0.5 border-2 border-line bg-night-950 p-0.5"
        role="group"
        aria-label={label}
      >
        {options.map((option) => (
          <button
            key={option.key}
            type="button"
            aria-pressed={value === option.key}
            className={`seg-tab ${
              value === option.key
                ? "bg-info font-bold text-[#04202a]"
                : "text-faint hover:bg-night-850 hover:text-ink-dim"
            }`}
            onClick={() => onChange(option.key)}
          >
            {option.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/**
 * A column header that doubles as a sort control. Clicking cycles through the
 * column's default direction, its reverse and the worker's ranked order.
 */
function SortableHeader({ column, sort, onSort, title, className = "px-3 py-2 font-medium" }) {
  const active = sort?.key === column.key;
  const dir = active ? sort.dir : column.defaultDir;
  return (
    <th
      scope="col"
      className={className}
      aria-sort={active ? (dir === "asc" ? "ascending" : "descending") : "none"}
      title={title}
    >
      <button
        type="button"
        className="group inline-flex items-center gap-1 whitespace-nowrap transition-colors hover:text-ink"
        onClick={() => onSort(column.key)}
        aria-label={`Sort by ${column.label}, currently ${active ? dir : "in the default ranking"}`}
        title={`${title || column.label} — click to sort${active ? ` (currently ${dir})` : ""}`}
      >
        {column.label}
        <Icon
          name={dir === "asc" ? "i-caret-up" : "i-caret-down"}
          className={`h-2 w-2 ${
            active
              ? "text-info"
              : "text-faint opacity-0 transition-opacity group-hover:opacity-100"
          }`}
        />
      </button>
    </th>
  );
}

/**
 * Placeholder shown where the table would be while the worker folds a fresh
 * window of frames. The padlock stays shut and the gear steps around it until
 * rows arrive; the shimmering bars preview the table that is coming.
 */
function ActivityLoading() {
  return (
    <div className="activity-loading" role="status" aria-live="polite">
      <div className="activity-lock" aria-hidden="true">
        <Icon name="i-gear" className="activity-gear" />
        <span className="lock-shackle" />
        <span className="lock-body" />
      </div>
      <p className="activity-loading-title">
        UNLOCKING MARKET FRAMES
        <span className="activity-dots" aria-hidden="true">
          <i />
          <i />
          <i />
        </span>
      </p>
      <p className="activity-loading-hint">
        Counting best asks per product in the selected range…
      </p>
      <div className="activity-loading-rows" aria-hidden="true">
        {[0, 1, 2, 3].map((index) => (
          <span className="skeleton activity-row-skeleton" key={index} />
        ))}
      </div>
    </div>
  );
}

/**
 * Ranked marketplace activity per item id.
 *
 * Every RLT market frame the worker sees is counted per item per hour, whether
 * or not the id is on the target fleet — that is the point of the panel: it
 * finds products worth watching instead of reporting on ones already watched.
 * Names are resolved by the worker from `item-info` and cached, so a row shows a
 * product name as soon as one is known and falls back to the short id until
 * then.
 */
export function ItemMonitor({ request, notify }) {
  const [range, setRange] = useState(DEFAULT_ACTIVITY_RANGE);
  const [collapsed, setCollapsed] = useCollapsed("item-activity");
  const panelRef = useRef(null);
  const [category, setCategory] = useState("");
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  // Separate draft/committed pair for the min-spread filter so typing a number
  // does not fire a request for every keystroke, same as the name search.
  const [spreadDraft, setSpreadDraft] = useState("");
  const [spread, setSpread] = useState(0);
  // Rows loaded so far, appended page by page like the loot log: the reader
  // keeps everything they have already seen and asks for more below it.
  const [rows, setRows] = useState([]);
  // `null` keeps the worker's ranking; a value re-sorts the rows on screen.
  const [sort, setSort] = useState(null);
  // Mirrors `rows` so the poll/refresh callbacks can read the current depth
  // without becoming dependencies of the load effect (which would reset it).
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  const [meta, setMeta] = useState(null);
  const [status, setStatus] = useState("loading");
  const [error, setError] = useState("");
  const [refreshedAt, setRefreshedAt] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  // The number of rows already held is both the next "Load more" offset and the
  // depth a refresh must re-read to keep those extra pages on screen.
  const requestRef = useRef(request);
  requestRef.current = request;
  // Guards against a stale in-flight page landing after the filters changed.
  const generationRef = useRef(0);
  // The mobile list and the desktop table are the same rows; render one.
  const wide = useMediaQuery("(min-width: 1024px)");

  useEffect(() => {
    const timer = setTimeout(() => setQuery(draft.trim()), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [draft]);

  useEffect(() => {
    const timer = setTimeout(() => {
      const value = Number(spreadDraft.trim());
      setSpread(Number.isFinite(value) && value > 0 ? value : 0);
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [spreadDraft]);

  const preset = ACTIVITY_RANGES.find((item) => item.key === range) || ACTIVITY_RANGES[0];

  const fetchPage = useCallback(
    async (offset, limit = ACTIVITY_PAGE_SIZE) => {
      // Recomputed per request instead of once per range choice: a rolling
      // window has to move with the clock, otherwise the 30 s poll would keep
      // re-reading the same frozen hour and the counts would never advance.
      const window = rangeForPreset(preset.key);
      const params = new URLSearchParams({
        since: String(window.since),
        until: String(window.until),
        limit: String(limit),
        offset: String(offset)
      });
      if (category) params.set("itemType", category);
      if (query) params.set("q", query);
      if (spread > 0) params.set("minSpreadPct", String(spread));
      const data = await requestRef.current(`/api/item-activity?${params.toString()}`);
      return { activity: data?.activity || {}, window };
    },
    [category, preset.key, query, spread]
  );

  const describe = useCallback(
    (activity, window) => ({
      totalEvents: Number(activity.totalEvents) || 0,
      totalRows: Number(activity.totalRows) || 0,
      typeTotals: Array.isArray(activity.typeTotals) ? activity.typeTotals : [],
      unresolved: Number(activity.unresolved) || 0,
      since: Number(activity.since) || window.since,
      until: Number(activity.until) || window.until
    }),
    []
  );

  // `preserveDepth` is the difference between a refetch and a reset. A filter
  // change starts again from page one; a manual refresh or a poll re-reads every
  // row already on screen, keeping the reader's load-more progress intact.
  const load = useCallback(
    async (preserveDepth = false) => {
      const generation = ++generationRef.current;
      try {
        let nextRows;
        let activity;
        let window;
        if (preserveDepth) {
          ({ rows: nextRows, activity, window } = await fetchActivityRows(
            fetchPage,
            rowsRef.current.length
          ));
        } else {
          ({ activity, window } = await fetchPage(0));
          nextRows = Array.isArray(activity.rows) ? activity.rows : [];
        }
        if (generation !== generationRef.current) return;
        setRows(nextRows);
        setMeta(describe(activity, window));
        setError("");
        setStatus("ready");
        setRefreshedAt(Date.now());
      } catch (caught) {
        if (generation !== generationRef.current) return;
        setError(caught?.message || "Could not load item activity.");
        // A failed poll must not blank a table that already has rows.
        setStatus((previous) => (previous === "loading" ? "error" : "stale"));
      }
    },
    [describe, fetchPage]
  );

  // Manual refresh gets the same accountable state as a filter change: the
  // button reports the work instead of looking inert while the request runs.
  const refresh = useCallback(() => {
    setStatus((previous) => (previous === "loading" ? "loading" : "refreshing"));
    load(true);
  }, [load]);

  // Appends the next page below what is already on screen.
  const loadMore = useCallback(async () => {
    const generation = generationRef.current;
    setLoadingMore(true);
    try {
      const { activity, window } = await fetchPage(rows.length);
      if (generation !== generationRef.current) return;
      const next = Array.isArray(activity.rows) ? activity.rows : [];
      setRows((previous) => {
        // A page is ranked, so an id can shift between requests. De-duplicating
        // by id keeps a product from appearing twice when that happens.
        const seen = new Set(previous.map((row) => row.itemId));
        return [...previous, ...next.filter((row) => !seen.has(row.itemId))];
      });
      setMeta(describe(activity, window));
      setError("");
    } catch (caught) {
      if (generation !== generationRef.current) return;
      setError(caught?.message || "Could not load more products.");
    } finally {
      // Always release the button: a refresh during this request changes the
      // generation, but the loader must not stay stuck on "Loading…".
      setLoadingMore(false);
    }
  }, [describe, fetchPage, rows.length]);

  useEffect(() => {
    let alive = true;
    generationRef.current += 1;
    setStatus((previous) => (previous === "ready" || previous === "stale" ? "refreshing" : "loading"));
    // The first mount and every filter change reset to the first page.
    load(false);
    const timer = setInterval(() => {
      // Polling a hidden tab wastes requests and battery on a phone. A visible
      // poll is a refetch, so it keeps the depth reached through "Load more".
      if (document.visibilityState === "visible" && alive) load(true);
    }, REFRESH_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [load]);

  // The category menu is built from the same range-wide type totals as the
  // header badges. Those totals are computed before the filters, so selecting
  // one category keeps every other category in the menu instead of making the
  // reader go back to "all" to find it again.
  const breakdown = useMemo(() => typeBreakdown(meta?.typeTotals), [meta]);
  const options = useMemo(() => {
    const menu = categoryOptions(meta?.typeTotals);
    // An old feed type that the current range no longer reports still needs an
    // option, otherwise the select would lose its own selected value.
    if (!category || menu.some((option) => option.key === category)) return menu;
    return [...menu, { key: category, count: 0 }];
  }, [category, meta]);
  // Denominator for the share column: every frame that matched the active
  // category/search filters, so the rows add up to about 100% and 180 of 2 176
  // reads 8% rather than 100%. It is also the basis of the "share" sort.
  const totalEvents = meta?.totalEvents ?? 0;
  // The worker ranks rows by frame count. An explicit sort reorders only the
  // rows currently loaded; "Load more" still appends by the worker's ranking
  // and the whole visible list is re-sorted afterwards.
  const filtered = useMemo(
    () => (sort ? sortActivityRows(rows, sort, { totalEvents }) : rows),
    [rows, sort, totalEvents]
  );
  const summary = useMemo(() => summarizeActivity(filtered), [filtered]);
  // The header summary comes from the range-wide totals, so it describes every
  // product in the window — not just the rows loaded so far.
  const totalRows = meta?.totalRows ?? 0;
  const shown = filtered.length;
  // The rows already held are the cursor of the next request; the helper keeps
  // the "is there more" rule in one place with the tests that cover it.
  const { hasMore } = nextBatch({ shown, total: totalRows });
  const loading = status === "loading";
  const empty = !loading && filtered.length === 0;
  // Follow the window the last response was computed for, so the caption
  // advances with each poll instead of freezing at the moment of the mount.
  const captionWindow = meta ?? rangeForPreset(preset.key);
  const caption = rangeCaption(captionWindow.since, captionWindow.until);

  const pickCategory = useCallback((value) => setCategory(value), []);
  const pickSort = useCallback((key) => setSort((current) => nextActivitySort(current, key)), []);
  const toggleSortDir = useCallback(
    () => setSort((current) => (current ? { ...current, dir: current.dir === "asc" ? "desc" : "asc" } : null)),
    []
  );

  const copyId = useCallback(
    async (row) => {
      const ok = await copyText(row.itemId);
      notify?.(
        ok ? "info" : "error",
        ok ? "ITEM ID COPIED" : "COPY FAILED",
        ok ? row.itemId : "Clipboard access was refused."
      );
    },
    [notify]
  );

  const live = status === "ready" || status === "refreshing";
  const refreshing = status === "refreshing";
  const stale = status === "stale";

  return (
    <section
      ref={panelRef}
      className={`panel min-w-0${collapsed ? " panel-collapsed" : ""}`}
      data-accent="cyan"
      aria-label="Item activity"
    >
      <div className="panel-head flex-wrap gap-x-3 gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-pulse" className="h-4 w-4 text-info" />
          Item activity
          <span
            className="border-2 border-[#05030f] bg-info px-1.5 py-0.5 font-mono text-[12px] font-bold text-[#04202a]"
            title={`${formatInt(meta?.totalRows ?? 0)} product${(meta?.totalRows ?? 0) === 1 ? "" : "s"} matched in this range`}
          >
            {formatInt(meta?.totalRows ?? 0)}
          </span>
        </h2>
        {/* Per-type frame totals for the whole range. This sits on the title
            row so the breakdown reads as a summary of the window rather than
            another column of the table. */}
        <div
          className={`flex min-w-0 flex-wrap items-center gap-1.5${collapsed ? " max-lg:hidden" : ""}`}
          role="group"
          aria-label="Frames per item type in the selected range, independent of the category, search and spread filters"
        >
          <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            Frames by type
          </span>
          {breakdown.map((total) => (
            <span
              key={total.key}
              className="type-badge gap-1"
              data-type={total.key}
              title={`${formatInt(total.events)} frames across ${formatInt(total.items)} product${total.items === 1 ? "" : "s"} in this range, regardless of the active filters`}
            >
              {itemTypeLabel(total.key)}
              <b className="tabular-nums">{formatInt(total.events)}</b>
            </span>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-2 sm:ml-auto">
          <div className={`flex flex-wrap items-center gap-2${collapsed ? " max-lg:hidden" : ""}`}>
            <Segmented label="Range" value={range} onChange={setRange} options={ACTIVITY_RANGES} />
            <button
              className="btn btn-ghost btn-sm"
              type="button"
              onClick={refresh}
              disabled={loading || refreshing}
              data-busy={refreshing ? "true" : undefined}
              aria-busy={refreshing}
              title={refreshing ? "Re-reading the worker…" : "Re-read the worker now"}
            >
              <Icon name="i-refresh" className="h-3 w-3" /> Refresh
            </button>
          </div>
          <CollapseToggle
            expanded={!collapsed}
            label="item activity"
            panelRef={panelRef}
            controls="item-activity-body"
            onToggle={() => setCollapsed((value) => !value)}
          />
        </div>
      </div>

      <div className="collapse-frame" id="item-activity-body" inert={collapsed || undefined}>
        <div className="collapse-body">
          <div className="flex flex-wrap items-center gap-2 border-b-2 border-line bg-night-950/40 px-4 py-2.5">
        {/* Phone: search, category and min-spread each take a full row. From
            `sm` up they sit inline on fixed-width controls, leaving the rest
            of the row for the counters. */}
        <div className="relative w-full min-w-0 sm:w-auto sm:shrink-0 sm:basis-[220px]">
          <Icon name="i-search" className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-faint" />
          <input
            className="input input-sm w-full pl-8"
            type="search"
            enterKeyHint="search"
            autoComplete="off"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") setDraft("");
            }}
            placeholder="Name or item id"
            aria-label="Search item activity by product name or item id"
          />
        </div>
        <select
          className="input input-sm w-full px-2 sm:w-auto sm:shrink-0"
          value={category}
          onChange={(event) => pickCategory(event.target.value)}
          aria-label="Filter item activity by product category"
          title="Every category stays listed; the counts cover the whole range regardless of the active filter"
        >
          <option value="">Category: all</option>
          {options.map((option) => (
            <option key={option.key} value={option.key}>
              {categoryLabel(option.key)} ({option.count})
            </option>
          ))}
        </select>
        <div className="relative w-full min-w-0 sm:w-auto sm:shrink-0 sm:basis-[130px]">
          <input
            className="input input-sm w-full pl-2 pr-7"
            type="number"
            min="0"
            step="any"
            inputMode="decimal"
            autoComplete="off"
            value={spreadDraft}
            onChange={(event) => setSpreadDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") setSpreadDraft("");
            }}
            placeholder="Min spread %"
            aria-label="Filter item activity by minimum min/max price distance in percent"
            title="Keep only products whose min/max price distance is at least this percent of the lowest price"
          />
          <span className="pointer-events-none absolute right-2.5 top-1/2 -translate-y-1/2 font-mono text-label text-faint">
            %
          </span>
        </div>
        {/* Sorting is client-side over the rows on screen, so it works on the
            phone list too (which has no column headers to click). */}
        <select
          className="input input-sm w-full px-2 sm:w-auto sm:shrink-0"
          value={sort?.key ?? ""}
          onChange={(event) => {
            const column = SORT_COLUMNS[event.target.value];
            setSort(column ? { key: column.key, dir: column.defaultDir } : null);
          }}
          aria-label="Sort item activity rows"
          title="Re-sort the rows currently shown; ranked keeps the worker's order"
        >
          <option value="">Sort: ranked</option>
          {ACTIVITY_SORT_COLUMNS.map((column) => (
            <option key={column.key} value={column.key}>
              Sort: {column.label}
            </option>
          ))}
        </select>
        {sort ? (
          <button
            className="btn btn-ghost btn-sm"
            type="button"
            onClick={toggleSortDir}
            aria-label={`Sort direction: ${sort.dir === "asc" ? "ascending" : "descending"}`}
            title={sort.dir === "asc" ? "Ascending — click for descending" : "Descending — click for ascending"}
          >
            <Icon name={sort.dir === "asc" ? "i-caret-up" : "i-caret-down"} className="h-2.5 w-2.5" />
            {sort.dir === "asc" ? "ASC" : "DESC"}
          </button>
        ) : null}
        <span className="flex flex-wrap items-center gap-2 font-mono text-[11px] uppercase tracking-[0.08em] text-faint sm:ml-auto">
          <span title="Every RLT market frame counted for the products matching the active filters in this range, including ids below the row cap">
            Frames{" "}
            <strong className="tabular-nums text-info">
              {formatInt(totalEvents || summary.total)}
            </strong>
          </span>
          <span className="text-line">·</span>
          <span title="Products in this table that are not on the target fleet">
            Off-fleet{" "}
            <strong className="tabular-nums text-gold">
              {formatInt(summary.untracked)}
            </strong>
          </span>
          <span className="hidden sm:inline text-line">·</span>
          <span className="hidden sm:inline">{caption}</span>
        </span>
      </div>

      {loading ? (
        <ActivityLoading />
      ) : empty ? (
        <div className="flex flex-col items-center justify-center gap-2 px-6 py-10 text-center">
          <Icon name="i-pulse" className="h-6 w-6 text-faint" />
          <p className="font-mono text-body uppercase tracking-[0.14em] text-ink-dim">
            {loading ? "Reading item activity…" : status === "error" ? "Item activity unavailable" : "No item activity in range"}
          </p>
          <p className="max-w-[52ch] text-body leading-relaxed text-faint">
            {loading
              ? "Counting the market frames the worker has seen."
              : status === "error"
                ? error
                : "The worker counts every RLT market frame per item id, so this fills as soon as the feed reports a product in the selected range and category."}
          </p>
        </div>
      ) : (
        <>
          {/* A poll or filter change keeps the old rows on screen, so a thin
              stepped scan line reports the work without hiding them. */}
          {refreshing ? (
            <div
              className="activity-scan"
              role="progressbar"
              aria-label="Refreshing item activity"
            />
          ) : null}
          {wide ? (
            <div className="report-scroll min-w-0">
              <table className="w-full min-w-[1080px] border-collapse text-left font-mono text-meta tabular-nums">
                <thead className="sticky top-0 z-10 bg-night-900">
                  <tr className="border-b-2 border-line text-label uppercase tracking-[0.08em] text-faint">
                    <SortableHeader column={SORT_COLUMNS.product} sort={sort} onSort={pickSort} className="px-4 py-2 font-medium" />
                    <SortableHeader column={SORT_COLUMNS.type} sort={sort} onSort={pickSort} title="Item type reported by the market feed or item-info" />
                    <SortableHeader column={SORT_COLUMNS.frames} sort={sort} onSort={pickSort} title="Market frames seen in the selected range and matching the active filters" />
                    <SortableHeader column={SORT_COLUMNS.share} sort={sort} onSort={pickSort} title="This product's share of all market frames that matched the active filters. Every row adds up to about 100%." />
                    <SortableHeader column={SORT_COLUMNS.minPrice} sort={sort} onSort={pickSort} title="Cheapest best ask seen in the selected range" />
                    <SortableHeader column={SORT_COLUMNS.maxPrice} sort={sort} onSort={pickSort} title="Dearest best ask seen in the selected range" />
                    <SortableHeader column={SORT_COLUMNS.spread} sort={sort} onSort={pickSort} title="Distance between max and min price as a percent of the min price: (max - min) / min x 100" />
                    <SortableHeader column={SORT_COLUMNS.events24h} sort={sort} onSort={pickSort} title="Frames seen in the last 24 hours, regardless of the range filter" />
                    <SortableHeader column={SORT_COLUMNS.lastSeen} sort={sort} onSort={pickSort} title="Most recent market frame seen for this product" />
                    <th scope="col" className="px-4 py-2 font-medium" title="Open the product on the Rollercoin marketplace">Market</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-line/40">
                  {filtered.map((row) => {
                    const name = rowLabel(row);
                    // This product's part of every frame counted in the range.
                    const share = sharePercent(row.events, totalEvents);
                    return (
                      <tr
                        className="transition-colors hover:bg-night-850/60"
                        key={row.itemId}
                        title={`item ${row.itemId}`}
                      >
                        <td className="max-w-[280px] px-4 py-2">
                          <span
                            className={`block truncate ${name.pending ? "text-muted" : "text-ink"}`}
                          >
                            {name.text}
                          </span>
                          <button
                            className="tap-pad -my-1 inline-flex min-h-6 items-center gap-1 whitespace-nowrap py-1 font-mono text-micro text-faint transition-colors hover:text-info"
                            type="button"
                            title="Copy item id"
                            onClick={() => copyId(row)}
                          >
                            <code>{row.itemId}</code>
                            <Icon name="i-copy" className="h-2.5 w-2.5" />
                          </button>
                        </td>
                        <td className="px-3 py-2">
                          {/* Same badge the target cards use, so a type reads
                              the same way on both panels. */}
                          <span className="type-badge" data-type={categoryOf(row)}>
                            {itemTypeLabel(categoryOf(row))}
                          </span>
                        </td>
                        <td className="px-3 py-2 font-semibold text-info">{formatInt(row.events)}</td>
                        <td className="px-3 py-2 tabular-nums text-ink-dim">{share}%</td>
                        <td
                          className="whitespace-nowrap px-3 py-2 text-gold"
                          title="Cheapest market price (best ask) seen for this product in the selected range"
                        >
                          {fixedPrice(row.minPrice)}
                        </td>
                        <td
                          className="whitespace-nowrap px-3 py-2 text-gold"
                          title="Dearest market price (best ask) seen for this product in the selected range"
                        >
                          {fixedPrice(row.maxPrice)}
                        </td>
                        <td
                          className="whitespace-nowrap px-3 py-2 font-semibold text-info"
                          title="(max price - min price) / min price x 100 in the selected range"
                        >
                          {formatPercent(priceSpreadPct(row), 1)}
                        </td>
                        <td className="px-3 py-2 text-muted">{formatInt(row.events24h)}</td>
                        <td
                          className="whitespace-nowrap px-3 py-2 text-faint"
                          title={row.lastAt ? new Date(row.lastAt).toLocaleString() : undefined}
                        >
                          {row.lastAt ? relTime(row.lastAt) : "—"}
                        </td>
                        <td className="px-4 py-2">
                          <a
                            className="icon-btn h-8 w-8"
                            href={rowMarketplaceUrl(row)}
                            target="_blank"
                            rel="noopener noreferrer"
                            title={`Open ${name.text} on the Rollercoin marketplace`}
                            aria-label={`Open ${name.text} on the Rollercoin marketplace`}
                          >
                            <Icon name="i-external" className="h-3.5 w-3.5" />
                          </a>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          ) : (
            <ul className="report-scroll divide-y divide-line/40">
              {filtered.map((row) => {
                const name = rowLabel(row);
                const share = sharePercent(row.events, totalEvents);
                return (
                  <li className="px-4 py-3" key={row.itemId}>
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <p className={`truncate text-body ${name.pending ? "text-muted" : "text-ink"}`}>
                          {name.text}
                        </p>
                        <div className="mt-1 flex items-center gap-2">
                          <span className="type-badge" data-type={categoryOf(row)}>
                            {itemTypeLabel(categoryOf(row))}
                          </span>
                          <code className="truncate font-mono text-micro text-faint">{row.itemId}</code>
                          <button
                            className="tap-pad inline-flex min-h-6 shrink-0 items-center py-1 text-faint transition-colors hover:text-info"
                            type="button"
                            title="Copy item id"
                            aria-label={`Copy item id ${row.itemId}`}
                            onClick={() => copyId(row)}
                          >
                            <Icon name="i-copy" className="h-2.5 w-2.5" />
                          </button>
                        </div>
                      </div>
                      <div className="shrink-0 text-right">
                        <p className="font-mono text-meta font-semibold tabular-nums text-info">
                          {formatInt(row.events)}
                        </p>
                        <p className="mt-0.5 font-mono text-label uppercase tracking-[0.08em] text-faint">
                          frames
                        </p>
                      </div>
                    </div>
                    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-micro tabular-nums">
                      <span className="text-gold" title="Cheapest best ask in this range">
                        MIN {fixedPrice(row.minPrice)}
                      </span>
                      <span className="text-gold" title="Dearest best ask in this range">
                        MAX {fixedPrice(row.maxPrice)}
                      </span>
                      <span
                        className="font-semibold text-info"
                        title="(max price - min price) / min price x 100 in this range"
                      >
                        SPREAD {formatPercent(priceSpreadPct(row), 1)}
                      </span>
                    </div>
                    <div className="mt-2 flex items-center gap-2">
                      <span className="shrink-0 font-mono text-micro tabular-nums text-ink-dim">
                        {share}% of range
                      </span>
                      <span className="min-w-0 shrink-0 font-mono text-micro uppercase tracking-[0.08em] text-faint">
                        24h {formatInt(row.events24h)}
                      </span>
                      <a
                        className="icon-btn h-9 w-9 shrink-0"
                        href={rowMarketplaceUrl(row)}
                        target="_blank"
                        rel="noopener noreferrer"
                        title={`Open ${name.text} on the Rollercoin marketplace`}
                        aria-label={`Open ${name.text} on the Rollercoin marketplace`}
                      >
                        <Icon name="i-external" className="h-3.5 w-3.5" />
                      </a>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </>
      )}

      <div className="flex shrink-0 items-center justify-between gap-2 border-t-2 border-line bg-night-900/70 px-3 py-3">
        <span role="status" className="min-w-0 truncate font-mono text-label font-semibold uppercase tracking-[0.08em] text-ink-dim">
          {shown === 0
            ? "No products"
            : hasMore
              ? `Showing ${formatInt(shown)} of ${formatInt(totalRows)}`
              : `${formatInt(totalRows)} products`}
          {summary.tracked > 0 ? ` · ${formatInt(summary.tracked)} on fleet` : ""}
        </span>
        <div className="flex shrink-0 items-center gap-2">
          {error ? (
            <span className="max-w-[220px] truncate font-mono text-label uppercase tracking-[0.08em] text-danger" title={error}>
              {stale ? "Stale · retrying" : "Load failed"}
            </span>
          ) : meta?.unresolved > 0 ? (
            <span
              className="font-mono text-micro uppercase tracking-[0.08em] text-faint"
              title="The worker is still resolving these names from item-info"
            >
              {formatInt(meta.unresolved)} unnamed
            </span>
          ) : null}
          <span
            className="flex items-center gap-1.5 font-mono text-micro uppercase tracking-[0.08em] text-faint"
            title={refreshedAt ? `Updated ${formatTime(refreshedAt)}` : undefined}
          >
            <span className={`led ${live ? "led-pulse bg-info" : "bg-faint"}`} />
            {refreshing ? "REFRESHING" : refreshedAt ? relTime(refreshedAt) : loading ? "LOADING" : "—"}
          </span>
          {/* Same affordance as the loot log: rows accumulate below and the
              reader asks for the next batch instead of jumping pages. */}
          {hasMore ? (
            <button
              className="btn btn-signal btn-sm font-bold"
              type="button"
              onClick={loadMore}
              disabled={loadingMore}
              data-busy={loadingMore ? "true" : undefined}
              aria-busy={loadingMore}
            >
              {loadingMore ? "Loading…" : "Load more"}
            </button>
          ) : null}
        </div>
      </div>
        </div>
      </div>
    </section>
  );
}
