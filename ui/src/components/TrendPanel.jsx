import { useMemo, useState } from "react";
import {
  fixedPrice,
  formatInt,
  itemTypeLabel,
  itemTypeOf,
  marketplaceUrl,
  shortId
} from "../lib/format.js";
import { Icon } from "./ModalShell.jsx";

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
  if (trend.source === "depth") {
    const units = Number.isFinite(trend.fillQty) ? formatInt(trend.fillQty) : null;
    return `Depth-weighted VWAP${units ? ` for ${units} units` : ""}${
      trend.fillApprox ? " (tail approximate)" : ""
    }`;
  }
  return "Top ask — depth history is still warming up";
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
        title={`${formatInt(trend.points)} samples · ${trend.confidence}% confidence`}
      >
        {trend.confidence}%
      </span>
    </div>
  );
}

function TrendRow({ row, onEdit }) {
  const { config, trend, target, market, proximity } = row;
  const meta = signalMeta(trend.state);
  const label = config.label || `ITEM ${shortId(config.itemId)}`;
  const momentum = Number(trend.momentumPct);
  const type = itemTypeOf(config);
  const gapTone = gapToneOf(proximity);

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
      <td className="numeric" title={trendSourceNote(trend)}>{fixedPrice(market)}</td>
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
      <td className={`numeric trend-conf-cell ${trend.reliable ? "" : "text-faint"}`}>
        {trend.confidence}%
      </td>
      <td>
        <span className="signal-chip" data-tone={meta.tone} title={meta.hint}>
          {meta.label}
        </span>
        {trend.reliable ? null : (
          <span className="ml-1 font-mono text-[10px] uppercase tracking-[0.08em] text-faint">
            low conf
          </span>
        )}
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
        <div className="px-4 py-8 text-center">
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
                <th scope="col" className="text-right">Gap</th>
                <th scope="col" className="text-right trend-mom">Mom</th>
                <th scope="col" className="text-right trend-conf-cell">Confidence</th>
                <th scope="col">Signal</th>
                <th scope="col" className="text-right">Action</th>
              </tr>
            </thead>
            <tbody>
              {current.map((row) => (
                <TrendRow key={row.config.itemId} row={row} onEdit={onEdit} />
              ))}
            </tbody>
          </table>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2">
        {Object.entries(SIGNAL_META)
          .filter(([key]) => ["ABOVE", "NEAR", "BELOW", "FAR_BELOW"].includes(key))
          .map(([key, meta]) => (
            <span key={key} className="signal-chip" data-tone={meta.tone} title={meta.hint}>
              {meta.label}
            </span>
          ))}
        <span className="font-mono text-[10px] uppercase tracking-[0.08em] text-faint">
          gap = target vs trend · mom = 15m/60m momentum
        </span>
      </div>
    </section>
  );
}
