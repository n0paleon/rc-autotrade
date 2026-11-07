import { useEffect, useMemo, useState } from "react";
import {
  fixedPrice,
  formatInt,
  itemTypeLabel,
  itemTypeOf,
  marketplaceUrl,
  relTime,
  shortId
} from "../lib/format.js";
import { Icon } from "./ModalShell.jsx";

/** Rows rendered at once; the fleet can be large and the table is scrollable. */
const TREND_BATCH = 12;

/**
 * Presentation-only vocabulary for the shared trend model. Keeping the labels
 * here means the worker never sends display strings and the panel/card agree.
 */
export const SIGNAL_META = Object.freeze({
  ABOVE: {
    label: "ABOVE TREND",
    tone: "danger",
    hint: "Target sits above the market trend: the bot would buy at market instead of on a dip."
  },
  NEAR: {
    label: "AT TREND",
    tone: "gold",
    hint: "Target is within a few percent of the trend, so the discount is thin."
  },
  BELOW: {
    label: "BELOW TREND",
    tone: "signal",
    hint: "Target is under the trend: the bot is waiting for a dip."
  },
  UNSURE: {
    label: "UNSURE",
    tone: "muted",
    hint: "The gap is smaller than the model's own uncertainty, so above/below cannot be resolved from the data on hand."
  },
  FAR_BELOW: {
    label: "FAR BELOW",
    tone: "info",
    hint: "Target is far under the trend, so fills may be rare."
  },
  STALE: {
    label: "STALE",
    tone: "muted",
    hint: "No fresh market frame; the signal is frozen on old data."
  },
  NO_DATA: {
    label: "NO DATA",
    tone: "muted",
    hint: "Not enough price history yet to draw a trend."
  },
  NO_TARGET: {
    label: "NO TARGET",
    tone: "muted",
    hint: "This target has no usable price ceiling."
  }
});

export function signalMeta(state) {
  return SIGNAL_META[state] || SIGNAL_META.NO_DATA;
}

/**
 * Why a row is not actionable. The model decides (`reliableReason`); the panel
 * only names it, and naming it matters because "low confidence" is the wrong
 * story when six hours of dense history sits behind a level that is simply too
 * loose to classify.
 */
const UNRELIABLE_NOTE = Object.freeze({
  "low-confidence": "low conf",
  "wide-band": "wide band",
  stale: "stale",
  "no-data": "no data",
  "no-target": "no target"
});

/**
 * The model's recommendation, rendered as one word. The advice is deliberately
 * narrow — lower a ceiling, leave it, look at it, or wait — because the only
 * thing the panel may never say is "buy".
 */
const ACTION_META = Object.freeze({
  lower: {
    label: "LOWER",
    tone: "danger",
    hint: "Target sits above the market level: a lower ceiling buys the same item on a dip."
  },
  hold: {
    label: "HOLD",
    tone: "signal",
    hint: "Nothing to change: the ceiling is on the healthy side of the level."
  },
  review: {
    label: "REVIEW",
    tone: "info",
    hint: "Target is far below the level, so fills may be rare; decide whether that is intended."
  },
  wait: {
    label: "WAIT",
    tone: "muted",
    hint: "The model will not stand behind this reading yet, so changing the target now would be guessing."
  },
  "set-target": {
    label: "SET",
    tone: "muted",
    hint: "This target has no usable price ceiling."
  }
});

/** Why the model said what it said; appended to the action chip tooltip. */
const ACTION_REASON = Object.freeze({
  "above-trend": "target is above the level",
  "below-trend": "target is under the level; waiting for a dip",
  "at-trend": "target is at the level, so the margin is thin",
  "far-below": "target is more than 20% under the level",
  unresolved: "the gap is smaller than the model's own uncertainty",
  "low-confidence": "too little price history to act on",
  "wide-band": "the level is too loose to act on",
  stale: "the feed has gone quiet",
  "no-data": "not enough history yet",
  "no-target": "no usable ceiling"
});

export function actionMeta(action) {
  return ACTION_META[action] || ACTION_META.wait;
}

export function actionNote(trend) {
  const reason = ACTION_REASON[trend?.actionReason];
  return reason ? `${actionMeta(trend?.action).hint} (${reason})` : actionMeta(trend?.action).hint;
}

/** Tailwind needs literal class names, so tone → utility is an explicit map. */
const TONE_TEXT = Object.freeze({
  danger: "text-danger",
  warn: "text-warn",
  gold: "text-gold",
  signal: "text-signal",
  info: "text-info",
  muted: "text-faint"
});

/**
 * Gap heat ramp: the closer the target sits to the trend line (zero), the
 * redder it reads, because that is where the next frame flips the signal.
 * Above-trend gaps stay red at every size — that side is always a bad entry.
 */
function gapToneOf(proximity) {
  if (!Number.isFinite(proximity) || proximity >= 0) return "danger";
  const distance = Math.abs(proximity);
  if (distance <= 2) return "danger";
  if (distance <= 5) return "warn";
  if (distance <= 10) return "gold";
  return "signal";
}

function momentumTone(value) {
  if (!Number.isFinite(value)) return "muted";
  if (value > 0.3) return "danger";
  if (value < -0.3) return "signal";
  return "muted";
}

/** Explains which series produced the trend, since the two are not equivalent. */
function trendSourceNote(trend) {
  const band = Number(trend.bandPct);
  const uncertainty = Number.isFinite(trend.bandPct) ? ` · level ±${band.toFixed(1)}%` : "";
  if (trend.source === "depth") {
    const units = Number.isFinite(trend.fillQty) ? formatInt(trend.fillQty) : null;
    return `Depth-weighted VWAP${units ? ` for ${units} units` : ""}${
      trend.fillApprox ? " (tail approximate)" : ""
    }${uncertainty}`;
  }
  return `Top ask — depth history is still warming up${uncertainty}`;
}

/**
 * Compact trend reading for a target card: trend level, distance of the target
 * from it, the classification, and model confidence.
 */
export function TrendChip({ trend }) {
  if (!trend || !Number.isFinite(trend.trend)) return null;
  const meta = signalMeta(trend.state);
  const proximity = Number(trend.proximityPct);
  const hasProximity = Number.isFinite(proximity);

  return (
    <div className="trend-strip" data-tone={meta.tone} title={`${meta.hint} · ${trendSourceNote(trend)}`}>
      <span className="label">Trend</span>
      <b>{fixedPrice(trend.trend)}</b>
      {hasProximity ? (
        <span className="trend-gap" data-dir={proximity >= 0 ? "up" : "down"}>
          <Icon
            name={proximity >= 0 ? "i-caret-up" : "i-caret-down"}
            className="h-2.5 w-2.5"
          />
          {Math.abs(proximity).toFixed(1)}%
        </span>
      ) : null}
      <span className="trend-signal">{meta.label}</span>
      <span
        className="trend-conf"
        title={`${formatInt(trend.points)} samples · ${trend.confidence}% confidence${
          Number.isFinite(trend.bandPct)
            ? ` · level ±${Number(trend.bandPct).toFixed(1)}%`
            : ""
        }`}
      >
        {trend.confidence}%
      </span>
    </div>
  );
}

function TrendRow({ row, onEdit }) {
  const { config, trend, target, market, marketPrice, marketSeenAt, proximity } = row;
  const meta = signalMeta(trend.state);
  const label = config.label || `ITEM ${shortId(config.itemId)}`;
  const momentum = Number(trend.momentumPct);
  const type = itemTypeOf(config);
  const gapTone = gapToneOf(proximity);
  const action = actionMeta(trend.action);

  return (
    <tr data-tone={meta.tone} data-reliable={trend.reliable ? "yes" : "no"}>
      <td>
        <div className="min-w-0">
          <span className="block truncate font-sans text-[13px] font-semibold text-ink" title={label}>
            {label}
          </span>
          <span className="block font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            {itemTypeLabel(type)} · {shortId(config.itemId)}
          </span>
        </div>
      </td>
      <td className="numeric">{fixedPrice(target)}</td>
      <td className="numeric" title={trendSourceNote(trend)}>
        <span className="block">{fixedPrice(market)}</span>
        {Number.isFinite(trend.bandPct) ? (
          <span
            className="block font-mono text-[10px] font-normal leading-tight text-faint"
            title={`The smoothed level may sit up to ±${Number(trend.bandPct).toFixed(1)}% from the live market; the signal band is drawn from that`}
          >
            ±{Number(trend.bandPct).toFixed(1)}%
          </span>
        ) : null}
      </td>
      <td
        className="numeric"
        title={
          Number.isFinite(marketSeenAt)
            ? `Live market price · frame ${relTime(marketSeenAt)}`
            : "No market frame yet"
        }
      >
        {Number.isFinite(marketPrice) ? fixedPrice(marketPrice) : "—"}
      </td>
      <td className={`numeric trend-gap-cell ${TONE_TEXT[gapTone]}`}>
        {proximity >= 0 ? "+" : ""}
        {proximity.toFixed(1)}%
      </td>
      <td className={`numeric trend-mom ${TONE_TEXT[momentumTone(momentum)]}`}>
        {Number.isFinite(momentum) ? (
          <span className="inline-flex items-center gap-1">
            <Icon
              name={momentum >= 0 ? "i-caret-up" : "i-caret-down"}
              className="h-2.5 w-2.5"
            />
            {Math.abs(momentum).toFixed(2)}%
          </span>
        ) : (
          "—"
        )}
      </td>
      <td
        className={`numeric trend-conf-cell ${trend.reliable ? "" : "text-faint"}`}
        title={`${formatInt(trend.points)} samples · ${trend.confidence}% confidence${
          Number.isFinite(trend.bandPct)
            ? ` · level ±${Number(trend.bandPct).toFixed(1)}%`
            : ""
        }`}
      >
        {trend.confidence}%
      </td>
      <td>
        <span className="signal-chip" data-tone={meta.tone} title={meta.hint}>
          {meta.label}
        </span>
        {trend.reliable ? null : (
          <span
            className="ml-1 font-mono text-[10px] uppercase tracking-[0.08em] text-faint"
            title={
              trend.reliableReason === "wide-band"
                ? "There is enough history, but the level is too loose for the gap to mean much"
                : "Not enough history for the reading to be actionable"
            }
          >
            {UNRELIABLE_NOTE[trend.reliableReason] ?? "unreliable"}
          </span>
        )}
      </td>
      <td>
        <span className="signal-chip" data-tone={action.tone} title={actionNote(trend)}>
          {action.label}
        </span>
      </td>
      <td>
        <div className="flex items-center justify-end gap-1">
          <a
            className="icon-btn h-8 w-8"
            href={marketplaceUrl(type, config.itemId)}
            target="_blank"
            rel="noopener noreferrer"
            title="Open on Rollercoin marketplace"
            aria-label={`Open ${label} on the Rollercoin marketplace`}
          >
            <Icon name="i-external" className="h-3 w-3" />
          </a>
          <button
            className="icon-btn h-8 w-8"
            type="button"
            title="Edit this target"
            aria-label={`Edit target ${label}`}
            onClick={() => onEdit(config)}
          >
            <Icon name="i-edit" className="h-3.5 w-3.5" />
          </button>
        </div>
      </td>
    </tr>
  );
}

/**
 * Fleet-level index of targets by their position against the trend.
 *
 * The default tab follows the workflow: every target whose ceiling is at or
 * above the market trend, largest gap first, because those are the ones that
 * need a follow-up. Below-trend targets are the healthy side and get their own
 * tab.
 */
export default function TrendPanel({ configs, onEdit }) {
  const [mode, setMode] = useState("above");
  const [limit, setLimit] = useState(TREND_BATCH);

  useEffect(() => {
    setLimit(TREND_BATCH);
  }, [mode]);

  const { rows, below, above, waiting, stale } = useMemo(() => {
    const usable = [];
    let missing = 0;
    let frozen = 0;

    for (const config of Array.isArray(configs) ? configs : []) {
      const trend = config?.runtime?.trend;
      const target = Number(config?.targetPrice);
      if (!trend || trend.status !== "ok" || !Number.isFinite(trend.trend)) {
        if (trend?.status === "stale") frozen += 1;
        else missing += 1;
        continue;
      }
      const proximity = Number(trend.proximityPct);
      if (!Number.isFinite(proximity)) {
        missing += 1;
        continue;
      }
      usable.push({
        config,
        trend,
        target,
        market: trend.trend,
        marketPrice: config?.runtime?.lastPrice,
        marketSeenAt: config?.runtime?.lastSeenAt,
        proximity
      });
    }

    const sorted = usable
      .slice()
      .sort((a, b) => b.proximity - a.proximity);

    return {
      rows: sorted,
      below: sorted.filter((row) => row.target < row.market),
      above: sorted.filter((row) => row.target >= row.market),
      waiting: missing,
      stale: frozen
    };
  }, [configs]);

  if (!Array.isArray(configs) || configs.length === 0) return null;

  const current = mode === "above" ? above : mode === "below" ? below : rows;
  const visible = current.slice(0, limit);
  const reliable = rows.filter((row) => row.trend.reliable).length;

  return (
    <section className="panel min-w-0" data-accent="gold" aria-label="Trend watch">
      <div className="panel-head flex-wrap gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-target" className="h-4 w-4 text-gold" />
          Trend watch
          <span className="border-2 border-[#05030f] bg-danger px-1.5 py-0.5 font-mono text-[12px] font-bold text-[#2b0008]">
            {above.length}
          </span>
        </h2>
        <div className="ml-auto flex w-full flex-wrap items-center gap-1.5 sm:w-auto" role="group" aria-label="Filter targets by trend position">
          <button
            className="trend-filter"
            type="button"
            data-tone="danger"
            aria-pressed={mode === "above"}
            onClick={() => setMode("above")}
          >
            at/above trend <span className="tabular-nums opacity-70">{above.length}</span>
          </button>
          <button
            className="trend-filter"
            type="button"
            data-tone="signal"
            aria-pressed={mode === "below"}
            onClick={() => setMode("below")}
          >
            below trend <span className="tabular-nums opacity-70">{below.length}</span>
          </button>
          <button
            className="trend-filter"
            type="button"
            data-tone="muted"
            aria-pressed={mode === "all"}
            onClick={() => setMode("all")}
          >
            all <span className="tabular-nums opacity-70">{rows.length}</span>
          </button>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1 border-b-2 border-line px-4 py-1.5">
        <p className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
          Signal only · nothing here places an order
        </p>
        <p className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
          {reliable} reliable
          {stale > 0 ? ` · ${stale} stale` : ""}
          {waiting > 0 ? ` · ${waiting} waiting for data` : ""}
        </p>
      </div>

      {current.length === 0 ? (
        <div className="px-4 py-5 text-center sm:py-8">
          <p className="font-mono text-[13px] font-semibold uppercase tracking-[0.14em] text-ink-dim">
            {mode === "above"
              ? "No target at or above trend"
              : mode === "below"
                ? "No target below trend"
                : "Waiting for market frames"}
          </p>
          <p className="mx-auto mt-1 max-w-md text-[13px] leading-relaxed text-faint">
            {mode === "above"
              ? "Every ceiling sits below the market trend — the healthy side, nothing to follow up."
              : mode === "below"
                ? "A ceiling appears here once it drops under its trend line; those are waiting for a dip."
                : "The worker needs a few minutes of price points before it can draw a trend."}
          </p>
        </div>
      ) : (
        <div
          className="trend-scroll"
          tabIndex={0}
          role="region"
          aria-label="Trend rows, scrollable"
        >
          <table className="trend-table">
            <caption className="sr-only">Targets compared with the market trend</caption>
            <thead>
              <tr>
                <th scope="col">Item</th>
                <th scope="col" className="text-right">Target</th>
                <th scope="col" className="text-right">Trend</th>
                <th scope="col" className="text-right">Market</th>
                <th scope="col" className="text-right">Gap</th>
                <th scope="col" className="text-right trend-mom">Mom</th>
                <th scope="col" className="text-right trend-conf-cell">Confidence</th>
                <th scope="col">Signal</th>
                <th scope="col">Next step</th>
                <th scope="col" className="text-right">Action</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((row) => (
                <TrendRow key={row.config.itemId} row={row} onEdit={onEdit} />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {current.length > visible.length ? (
        <div className="flex items-center justify-between gap-2 border-t-2 border-line px-4 py-2">
          <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            Showing {visible.length} of {current.length}
          </span>
          <button
            className="btn btn-ghost btn-sm"
            type="button"
            onClick={() => setLimit((value) => value + TREND_BATCH)}
          >
            Show {Math.min(current.length - visible.length, TREND_BATCH)} more
            <Icon name="i-arrow-down" className="h-3 w-3" />
          </button>
        </div>
      ) : null}

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2">
        {Object.entries(SIGNAL_META)
          .filter(([key]) => ["ABOVE", "NEAR", "UNSURE", "BELOW", "FAR_BELOW"].includes(key))
          .map(([key, meta]) => (
            <span key={key} className="signal-chip" data-tone={meta.tone} title={meta.hint}>
              {meta.label}
            </span>
          ))}
        <span className="font-mono text-[10px] uppercase tracking-[0.08em] text-faint">
          gap = target vs trend · mom = 15m/60m momentum · ± = level uncertainty
        </span>
        <span className="ml-auto flex flex-wrap items-center gap-1.5">
          {["lower", "hold", "review", "wait"].map((key) => (
            <span
              key={key}
              className="signal-chip"
              data-tone={ACTION_META[key].tone}
              title={ACTION_META[key].hint}
            >
              {ACTION_META[key].label}
            </span>
          ))}
          <span className="font-mono text-[10px] uppercase tracking-[0.08em] text-faint">
            next step
          </span>
        </span>
      </div>
    </section>
  );
}
