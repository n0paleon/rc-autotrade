import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  ACTIVITY_PAGE_SIZE,
  ACTIVITY_RANGES,
  categoryLabel,
  categoryOf,
  DEFAULT_ACTIVITY_RANGE,
  nextBatch,
  rangeForPreset,
  rangeSpansDays,
  rowLabel,
  rowMarketplaceUrl,
  sharePercent,
  summarizeActivity,
  typeBreakdown
} from "../lib/activity.js";
import { formatInt, formatTime, itemTypeLabel, relTime } from "../lib/format.js";
import { useMediaQuery } from "../lib/hooks.js";
import { Icon } from "./ModalShell.jsx";

/** How often the table re-reads the worker while the tab is visible. */
const REFRESH_MS = 30_000;
const SEARCH_DEBOUNCE_MS = 250;

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
        role="tablist"
        aria-label={label}
      >
        {options.map((option) => (
          <button
            key={option.key}
            type="button"
            role="tab"
            aria-selected={value === option.key}
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
  const [category, setCategory] = useState("");
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  // Rows loaded so far, appended page by page like the loot log: the reader
  // keeps everything they have already seen and asks for more below it.
  const [rows, setRows] = useState([]);
  const [meta, setMeta] = useState(null);
  const [status, setStatus] = useState("loading");
  const [error, setError] = useState("");
  const [refreshedAt, setRefreshedAt] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  // The first page is replaced on every poll; later pages are only appended on
  // demand. Tracking "how many rows we already hold" is what turns the page API
  // into a load-more cursor.
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

  const preset = ACTIVITY_RANGES.find((item) => item.key === range) || ACTIVITY_RANGES[0];
  // The window is derived from the selected preset, so a refresh (timer or the
  // manual button) always re-reads the range the user is looking at.
  const window = useMemo(() => rangeForPreset(preset.key), [preset.key]);

  const fetchPage = useCallback(
    async (offset) => {
      const params = new URLSearchParams({
        since: String(window.since),
        until: String(window.until),
        limit: String(ACTIVITY_PAGE_SIZE),
        offset: String(offset)
      });
      if (category) params.set("itemType", category);
      if (query) params.set("q", query);
      const data = await requestRef.current(`/api/item-activity?${params.toString()}`);
      return data?.activity || {};
    },
    [category, query, window.since, window.until]
  );

  const describe = useCallback(
    (activity) => ({
      totalEvents: Number(activity.totalEvents) || 0,
      totalRows: Number(activity.totalRows) || 0,
      typeTotals: Array.isArray(activity.typeTotals) ? activity.typeTotals : [],
      unresolved: Number(activity.unresolved) || 0,
      since: Number(activity.since) || window.since,
      until: Number(activity.until) || window.until
    }),
    [window.since, window.until]
  );

  // Refreshes the first page in place. Any extra pages the reader loaded are
  // dropped: their contents are a snapshot of an older moment, and keeping them
  // under a fresh head would mix two different counts in one table.
  const load = useCallback(async () => {
    const generation = ++generationRef.current;
    try {
      const activity = await fetchPage(0);
      if (generation !== generationRef.current) return;
      setRows(Array.isArray(activity.rows) ? activity.rows : []);
      setMeta(describe(activity));
      setError("");
      setStatus("ready");
      setRefreshedAt(Date.now());
    } catch (caught) {
      if (generation !== generationRef.current) return;
      setError(caught?.message || "Could not load item activity.");
      // A failed poll must not blank a table that already has rows.
      setStatus((previous) => (previous === "loading" ? "error" : "stale"));
    }
  }, [describe, fetchPage]);

  // Appends the next page below what is already on screen.
  const loadMore = useCallback(async () => {
    const generation = generationRef.current;
    setLoadingMore(true);
    try {
      const activity = await fetchPage(rows.length);
      if (generation !== generationRef.current) return;
      const next = Array.isArray(activity.rows) ? activity.rows : [];
      setRows((previous) => {
        // A page is ranked, so an id can shift between requests. De-duplicating
        // by id keeps a product from appearing twice when that happens.
        const seen = new Set(previous.map((row) => row.itemId));
        return [...previous, ...next.filter((row) => !seen.has(row.itemId))];
      });
      setMeta(describe(activity));
      setError("");
    } catch (caught) {
      if (generation !== generationRef.current) return;
      setError(caught?.message || "Could not load more products.");
    } finally {
      if (generation === generationRef.current) setLoadingMore(false);
    }
  }, [describe, fetchPage, rows.length]);

  useEffect(() => {
    let alive = true;
    generationRef.current += 1;
    setStatus((previous) => (previous === "ready" || previous === "stale" ? "refreshing" : "loading"));
    const run = async () => {
      if (!alive) return;
      await load();
    };
    run();
    const timer = setInterval(() => {
      // Polling a hidden tab wastes requests and battery on a phone.
      if (document.visibilityState === "visible") run();
    }, REFRESH_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [load]);

  // The category menu must list every type in the range, not just the types on
  // the current page — otherwise paging would silently change the filters the
  // reader can pick. `typeTotals` is computed over the whole range for exactly
  // this reason.
  const options = useMemo(
    () =>
      (meta?.typeTotals || [])
        .map((total) => ({ key: String(total.itemType || "unknown"), count: Number(total.items) || 0 }))
        .filter((option) => option.count > 0)
        .sort((a, b) => b.count - a.count || a.key.localeCompare(b.key)),
    [meta]
  );
  // Filtering and ranking happen in the worker, so the rows it returns are
  // already ordered.
  const filtered = rows;
  const summary = useMemo(() => summarizeActivity(filtered), [filtered]);
  // The header summary comes from the range-wide totals, so it describes every
  // product in the window — not just the rows loaded so far.
  const breakdown = useMemo(() => typeBreakdown(meta?.typeTotals), [meta]);
  const totalRows = meta?.totalRows ?? 0;
  const shown = filtered.length;
  // The rows already held are the cursor of the next request; the helper keeps
  // the "is there more" rule in one place with the tests that cover it.
  const { hasMore } = nextBatch({ shown, total: totalRows });
  // Denominator for the share column: every frame counted in the range, so the
  // rows add up to about 100% and 180 of 2 176 reads 8% rather than 100%.
  const totalEvents = meta?.totalEvents ?? 0;
  const loading = status === "loading";
  const empty = !loading && filtered.length === 0;
  const caption = rangeCaption(window.since, window.until);

  const pickCategory = useCallback((value) => setCategory(value), []);

  const copyId = useCallback(
    async (row) => {
      try {
        await navigator.clipboard.writeText(row.itemId);
        notify?.("info", "ITEM ID COPIED", row.itemId);
      } catch {
        notify?.("error", "COPY FAILED", "Clipboard access was refused.");
      }
    },
    [notify]
  );

  const live = status === "ready" || status === "refreshing";
  const stale = status === "stale";

  return (
    <section className="panel min-w-0" data-accent="cyan" aria-label="Item activity">
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
          className="flex min-w-0 flex-wrap items-center gap-1.5"
          aria-label="Frames per item type in the selected range"
        >
          {breakdown.length === 0 ? null : (
            <>
              <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
                Frames by type
              </span>
              {breakdown.map((total) => (
                <span
                  key={total.key}
                  className="type-badge gap-1"
                  data-type={total.key}
                  title={`${formatInt(total.events)} frames across ${formatInt(total.items)} product${total.items === 1 ? "" : "s"}`}
                >
                  {itemTypeLabel(total.key)}
                  <b className="tabular-nums">{formatInt(total.events)}</b>
                </span>
              ))}
            </>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2 sm:ml-auto">
          <Segmented label="Range" value={range} onChange={setRange} options={ACTIVITY_RANGES} />
          <button
            className="btn btn-ghost btn-sm"
            type="button"
            onClick={load}
            disabled={loading}
            title="Re-read the worker now"
          >
            <Icon name="i-refresh" className="h-3 w-3" /> Refresh
          </button>
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-2 border-b-2 border-line bg-night-950/40 px-4 py-2.5">
        {/* Phone: search and the category select each take a full row, so the
            select cannot slide over the search icon. From `sm` up they sit
            inline on a fixed 220 px search, leaving room for the counters. */}
        <div className="relative w-full min-w-0 sm:w-auto sm:shrink-0 sm:basis-[220px]">
          <Icon name="i-search" className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-faint" />
          <input
            className="input input-sm w-full pl-8"
            type="search"
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
          title="Only products the feed has reported in the current range are listed"
        >
          <option value="">Category: all</option>
          {options.map((option) => (
            <option key={option.key} value={option.key}>
              {categoryLabel(option.key)} ({option.count})
            </option>
          ))}
        </select>
        <span className="flex flex-wrap items-center gap-2 font-mono text-[11px] uppercase tracking-[0.08em] text-faint sm:ml-auto">
          <span title="Every RLT market frame counted for every item id in this range, including ids below the row cap">
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

      {empty ? (
        <div className="flex flex-col items-center justify-center gap-2 px-6 py-10 text-center">
          <Icon name="i-pulse" className="h-6 w-6 text-faint" />
          <p className="font-mono text-[13px] uppercase tracking-[0.14em] text-ink-dim">
            {loading ? "Reading item activity…" : status === "error" ? "Item activity unavailable" : "No item activity in range"}
          </p>
          <p className="max-w-[52ch] text-[13px] text-faint">
            {loading
              ? "Counting the market frames the worker has seen."
              : status === "error"
                ? error
                : "The worker counts every RLT market frame per item id, so this fills as soon as the feed reports a product in the selected range and category."}
          </p>
        </div>
      ) : (
        <>
          {wide ? (
            <div className="report-scroll min-w-0">
              <table className="w-full min-w-[820px] border-collapse text-left font-mono text-[13px] tabular-nums">
                <thead className="sticky top-0 z-10 bg-night-900">
                  <tr className="border-b-2 border-line text-[12px] uppercase tracking-[0.08em] text-faint">
                    <th scope="col" className="px-4 py-2 font-medium">Product</th>
                    <th scope="col" className="px-3 py-2 font-medium" title="Item type reported by the market feed or item-info">Type</th>
                    <th scope="col" className="px-3 py-2 font-medium" title="Market frames seen in the selected range">Frames</th>
                    <th scope="col" className="px-3 py-2 font-medium" title="This product's share of all market frames counted in the range. Every row of the range adds up to about 100%.">Share</th>
                    <th scope="col" className="px-3 py-2 font-medium" title="Frames seen in the last 24 hours, regardless of the range filter">Last 24h</th>
                    <th scope="col" className="px-3 py-2 font-medium">Last seen</th>
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
                            className="tap-pad -my-1 inline-flex min-h-6 items-center gap-1 whitespace-nowrap py-1 font-mono text-[11px] text-faint transition-colors hover:text-info"
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
                        <p className={`truncate text-[13px] ${name.pending ? "text-muted" : "text-ink"}`}>
                          {name.text}
                        </p>
                        <div className="mt-1 flex items-center gap-2">
                          <span className="type-badge" data-type={categoryOf(row)}>
                            {itemTypeLabel(categoryOf(row))}
                          </span>
                          <code className="truncate font-mono text-[11px] text-faint">{row.itemId}</code>
                        </div>
                      </div>
                      <div className="shrink-0 text-right">
                        <p className="font-mono text-[13px] font-semibold tabular-nums text-info">
                          {formatInt(row.events)}
                        </p>
                        <p className="mt-0.5 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
                          frames
                        </p>
                      </div>
                    </div>
                    <div className="mt-2 flex items-center gap-2">
                      <span className="shrink-0 font-mono text-[11px] tabular-nums text-ink-dim">
                        {share}% of range
                      </span>
                      <span className="min-w-0 shrink-0 font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
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
        <span className="min-w-0 truncate font-mono text-[12px] font-semibold uppercase tracking-[0.08em] text-ink-dim">
          {shown === 0
            ? "No products"
            : hasMore
              ? `Showing ${formatInt(shown)} of ${formatInt(totalRows)}`
              : `${formatInt(totalRows)} products`}
          {summary.tracked > 0 ? ` · ${formatInt(summary.tracked)} on fleet` : ""}
        </span>
        <div className="flex shrink-0 items-center gap-2">
          {error ? (
            <span className="max-w-[220px] truncate font-mono text-[12px] uppercase tracking-[0.08em] text-danger" title={error}>
              {stale ? "Stale · retrying" : "Load failed"}
            </span>
          ) : meta?.unresolved > 0 ? (
            <span
              className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint"
              title="The worker is still resolving these names from item-info"
            >
              {formatInt(meta.unresolved)} unnamed
            </span>
          ) : null}
          <span
            className="flex items-center gap-1.5 font-mono text-[11px] uppercase tracking-[0.08em] text-faint"
            title={refreshedAt ? `Updated ${formatTime(refreshedAt)}` : undefined}
          >
            <span className={`led ${live ? "led-pulse bg-info" : "bg-faint"}`} />
            {refreshedAt ? relTime(refreshedAt) : loading ? "LOADING" : "—"}
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
    </section>
  );
}
