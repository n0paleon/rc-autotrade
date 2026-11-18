/**
 * Feed connection history for the monitor window.
 *
 * The worker keeps a small in-memory ring of connect / disconnect / reconnect
 * transitions and ships it with the gateway telemetry. Every disconnect carries
 * a `sessionMs` duration, so these helpers never have to guess how long a link
 * was up before it ended. This module is pure so the table stays easy to test.
 */

import { formatUptime } from "./format.js";

/** Deck labels and chip colours for each lifecycle event. */
export const CONNECTION_EVENT_META = Object.freeze({
  connect: { label: "Connect", chip: "chip-online", tone: "text-signal" },
  reconnect: { label: "Reconnect", chip: "chip-dry", tone: "text-info" },
  disconnect: { label: "Disconnect", chip: "chip-offline", tone: "text-danger" }
});

/** Fixed table height: the log is capped at 100 rows, the deck shows 10 at a time. */
export const CONNECTION_LOG_PAGE_SIZE = 10;

const FALLBACK_META = Object.freeze({ label: "Unknown", chip: "", tone: "text-faint" });

export function connectionEventMeta(kind) {
  return CONNECTION_EVENT_META[kind] ?? FALLBACK_META;
}

/** Local wall-clock stamp for the time column. */
export function formatClock(ts) {
  if (!Number.isFinite(ts)) return "—";
  return new Date(ts).toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false
  });
}

/** Format a session length the same way the link-uptime counter reads. */
export function formatSessionDuration(ms) {
  if (ms == null || !Number.isFinite(Number(ms))) return "—";
  return formatUptime(Number(ms));
}

/**
 * Turn the worker's newest-first log into normalized, newest-first table rows.
 * Malformed entries are kept (as "Unknown") rather than silently dropped, so a
 * schema change on the worker is visible instead of hiding connection churn.
 */
export function connectionLogRows(log) {
  if (!Array.isArray(log)) return [];

  return log
    .map((raw, index) => {
      const kind = typeof raw?.kind === "string" ? raw.kind : "unknown";
      const at = Number(raw?.at);
      // A disconnect must always show its session duration; other events do
      // not have one and render a dash instead.
      const sessionMs =
        kind === "disconnect" && Number.isFinite(Number(raw?.sessionMs))
          ? Math.max(0, Math.round(Number(raw.sessionMs)))
          : null;
      const meta = connectionEventMeta(kind);
      return {
        id: `${Number.isFinite(at) ? at : 0}-${kind}-${index}`,
        at: Number.isFinite(at) ? at : null,
        kind,
        label: meta.label,
        chip: meta.chip,
        tone: meta.tone,
        sessionMs,
        sessionLabel: sessionMs == null ? "—" : formatSessionDuration(sessionMs),
        detail: typeof raw?.reason === "string" && raw.reason ? raw.reason : "—"
      };
    })
    .sort((a, b) => (b.at ?? 0) - (a.at ?? 0));
}

/**
 * Slice normalized rows into one fixed-size page. The requested page is
 * clamped to the available range, so a fresh telemetry push or a shorter log
 * can never leave the table pointing at an empty page.
 */
export function connectionLogPage(
  rows,
  requestedPage = 0,
  pageSize = CONNECTION_LOG_PAGE_SIZE
) {
  const list = Array.isArray(rows) ? rows : [];
  const size =
    Number.isFinite(pageSize) && pageSize > 0
      ? Math.floor(pageSize)
      : CONNECTION_LOG_PAGE_SIZE;
  const total = list.length;
  const pageCount = Math.max(1, Math.ceil(total / size));
  const requested = Number(requestedPage);
  const page = Math.min(
    Math.max(0, Number.isFinite(requested) ? Math.floor(requested) : 0),
    pageCount - 1
  );
  const start = page * size;

  return {
    rows: list.slice(start, start + size),
    page,
    pageCount,
    total,
    from: total === 0 ? 0 : start + 1,
    to: Math.min(start + size, total)
  };
}
