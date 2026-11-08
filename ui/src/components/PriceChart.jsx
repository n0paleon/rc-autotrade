import { useEffect, useMemo, useRef, useState } from "react";
import {
  Area,
  CartesianGrid,
  ComposedChart,
  Line,
  LineChart,
  ReferenceDot,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import { fixedPrice, formatInt, formatTime, shortId, trimPrice } from "../lib/format.js";
import {
  breakPriceGaps,
  bucketPrice,
  mergePriceSeries,
  PRICE_BUCKETS,
  PRICE_PERIODS,
  summarizePrice,
  timezoneOffsetMs
} from "../lib/price.js";
import { ModalShell } from "./ModalShell.jsx";

/**
 * The card chart is a compact "is this target in range" signal. The detail
 * modal is a proper price series: no bars to shrink at high resolution, price
 * axis on the right, crosshair readout, and target distance in numbers.
 */
const PRO = Object.freeze({
  up: "#2ee6a8",
  down: "#ff5d73",
  target: "#f5b544",
  grid: "#16202b",
  axis: "#5d6b7f",
  crosshair: "#3d4d61"
});

function pad(value) {
  return String(value).padStart(2, "0");
}

function day(ts) {
  return new Date(ts).toLocaleDateString("en-GB", { day: "2-digit", month: "short" });
}

function rangeLabel(start, bucketMs) {
  const from = new Date(start);
  const to = new Date(start + bucketMs);
  const sameDay =
    from.getDate() === to.getDate() &&
    from.getMonth() === to.getMonth() &&
    from.getFullYear() === to.getFullYear();
  const clock = (value) => `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  if (sameDay) return `${clock(from)}–${clock(to)}`;
  return `${day(start)} ${clock(from)}–${day(start + bucketMs)} ${clock(to)}`;
}

function axisTick(value, days, bucketMs) {
  const date = new Date(value);
  if (bucketMs >= 86_400_000) return `${date.getDate()}/${date.getMonth() + 1}`;
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (days <= 1) return clock;
  return `${date.getDate()}/${date.getMonth() + 1} ${clock}`;
}

const TICK_STEPS = Object.freeze([
  5 * 60_000,
  15 * 60_000,
  30 * 60_000,
  60 * 60_000,
  2 * 60 * 60_000,
  3 * 60 * 60_000,
  6 * 60 * 60_000,
  12 * 60 * 60_000,
  86_400_000,
  2 * 86_400_000
]);

/** Local-clock ticks sized so three to six labels fit the wide chart. */
function timeTicks(start, end) {
  const span = end - start;
  if (!Number.isFinite(span) || span <= 0) return undefined;
  const step = TICK_STEPS.find((size) => span / size <= 5) ?? 7 * 86_400_000;
  const offset = timezoneOffsetMs();
  const first = Math.ceil((start + offset) / step) * step - offset;
  const ticks = [];
  for (let at = first; at <= end; at += step) ticks.push(at);
  return ticks.length >= 2 ? ticks : undefined;
}

function ChartTooltip({ active, payload, label, bucketMs }) {
  if (!active || !payload?.length) return null;
  return (
    <div className="chart-tip">
      <span>
        {bucketMs > 60_000 ? rangeLabel(label, bucketMs) : formatTime(label).slice(0, 5)}
      </span>
      <strong>{fixedPrice(payload[0].value)} RLT</strong>
    </div>
  );
}

function ExpandIcon({ className = "h-3 w-3" }) {
  return (
    <svg
      viewBox="0 0 16 16"
      className={className}
      aria-hidden="true"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="square"
    >
      <path d="M2 6V2h4M2 2l4 4M14 10v4h-4M14 14l-4-4" />
    </svg>
  );
}

/** Neutral, chart-first tabs for the detail view — not the arcade segmented control. */
function ProTabs({ label, options, value, onChange }) {
  return (
    <div className="flex items-center gap-2">
      <span className="font-mono text-[9px] uppercase tracking-[0.16em] text-[#5d6b7f]">{label}</span>
      <div
        className="flex items-center gap-px border border-[#1d2a38] bg-[#0d141d] p-px"
        role="tablist"
        aria-label={label}
      >
        {options.map((option) => (
          <button
            key={option.key}
            type="button"
            role="tab"
            aria-selected={value === option.key}
            className={`h-6 min-w-7 px-2 font-mono text-[10px] uppercase tracking-[0.06em] transition-colors ${
              value === option.key
                ? "bg-[#1e2c3d] text-[#e8eef7]"
                : "text-[#6f8199] hover:bg-[#121b26] hover:text-[#c6d1e0]"
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

function ProTooltip({ active, payload, previousClose, bucketMs, targetPrice }) {
  if (!active || !payload?.length) return null;
  const point = payload[0].payload;
  if (!Number.isFinite(point?.p)) return null;
  const previous = previousClose?.get(point.t) ?? null;
  const delta =
    Number.isFinite(previous) && previous > 0 ? ((point.p - previous) / previous) * 100 : null;
  const toTarget =
    Number.isFinite(targetPrice) && targetPrice > 0 && point.p > 0
      ? ((targetPrice - point.p) / point.p) * 100
      : null;
  const low = Number.isFinite(point?.l) ? point.l : null;
  const high = Number.isFinite(point?.h) ? point.h : null;
  const bandPct =
    low != null && high != null && low > 0 ? ((high - low) / low) * 100 : null;

  return (
    <div className="pro-tip">
      <p className="pro-tip-range">
        {bucketMs > 60_000 ? rangeLabel(point.t, bucketMs) : formatTime(point.t).slice(0, 5)}
      </p>
      <div className="pro-tip-row">
        <span>Close</span>
        <strong>{fixedPrice(point.p)} RLT</strong>
      </div>
      {low != null && high != null ? (
        <>
          <div className="pro-tip-row">
            <span>Low</span>
            <strong>{fixedPrice(low)} RLT</strong>
          </div>
          <div className="pro-tip-row">
            <span>High</span>
            <strong>{fixedPrice(high)} RLT</strong>
          </div>
          {bandPct != null ? (
            <div className="pro-tip-row">
              <span>Band</span>
              <b className="text-[#8fa0b8]">{bandPct.toFixed(2)}%</b>
            </div>
          ) : null}
        </>
      ) : null}
      <div className="pro-tip-row">
        <span>vs previous</span>
        <b style={{ color: delta != null && delta < 0 ? PRO.down : PRO.up }}>
          {delta == null ? "—" : `${delta >= 0 ? "+" : ""}${delta.toFixed(2)}%`}
        </b>
      </div>
      {toTarget != null ? (
        <div className="pro-tip-row">
          <span>To target</span>
          <b className={toTarget >= 0 ? "text-[#f5b544]" : "text-[#8fa0b8]"}>
            {`${toTarget >= 0 ? "+" : ""}${toTarget.toFixed(2)}%`}
          </b>
        </div>
      ) : null}
    </div>
  );
}

/**
 * Vertical domain for a price series. The target joins the domain only when it
 * sits within two spans of the visible prices; a far-away order ceiling would
 * otherwise flatten the series into a line at the top of the plot.
 */
function targetDomain(prices, targetPrice, padRatio = 0.12) {
  const rawMin = prices.length ? Math.min(...prices) : 0;
  const rawMax = prices.length ? Math.max(...prices) : 1;
  const rawSpan = rawMax - rawMin || Math.max(rawMax * 0.005, 0.000001);
  const showTarget =
    prices.length > 0 &&
    Number.isFinite(targetPrice) &&
    targetPrice >= rawMin - rawSpan * 2 &&
    targetPrice <= rawMax + rawSpan * 2;
  let min = showTarget ? Math.min(rawMin, targetPrice) : rawMin;
  let max = showTarget ? Math.max(rawMax, targetPrice) : rawMax;
  if (!(max > min)) {
    const padding = Math.max(min * 0.01, 0.000001);
    min -= padding;
    max += padding;
  }
  const pad = (max - min) * padRatio;
  return { domain: [min - pad, max + pad], showTarget };
}

function PriceDetailModal({
  label,
  itemId,
  data,
  period,
  bucket,
  days,
  frame,
  periodLabel,
  targetPrice,
  status,
  error,
  overlay,
  onClose,
  onPeriod,
  onBucket
}) {
  const { first, last, change, high, low } = useMemo(() => summarizePrice(data), [data]);
  const chartData = useMemo(() => breakPriceGaps(data, frame.ms), [data, frame.ms]);
  const previousClose = useMemo(() => {
    const byTime = new Map();
    let previous = null;
    for (const point of data) {
      if (!Number.isFinite(point?.p)) continue;
      byTime.set(point.t, previous);
      previous = point.p;
    }
    return byTime;
  }, [data]);
  const ticks = useMemo(
    () => (data.length > 1 ? timeTicks(data[0].t, data[data.length - 1].t) : undefined),
    [data]
  );
  const values = data.flatMap((point) => {
    const out = [point.p];
    if (Number.isFinite(point.l)) out.push(point.l);
    if (Number.isFinite(point.h)) out.push(point.h);
    return out;
  });
  const hasBand = data.some(
    (point) => Number.isFinite(point.l) && Number.isFinite(point.h)
  );
  const { domain, showTarget } = targetDomain(values, targetPrice);
  const xDomain =
    data.length > 1
      ? ["dataMin", "dataMax"]
      : data.length === 1
        ? [data[0].t - frame.ms / 2, data[0].t + frame.ms / 2]
        : ["dataMin", "dataMax"];

  const color = (change ?? 0) >= 0 ? PRO.up : PRO.down;
  const inZone = last != null && Number.isFinite(targetPrice) ? last.p <= targetPrice : false;
  const gapPct =
    last != null && Number.isFinite(targetPrice) && last.p > 0
      ? ((targetPrice - last.p) / last.p) * 100
      : null;
  const rangePct = high > 0 ? ((high - low) / high) * 100 : null;

  const metrics = [
    ["Open", first ? fixedPrice(first.p) : "—"],
    ["High", high != null ? fixedPrice(high) : "—"],
    ["Low", low != null ? fixedPrice(low) : "—"],
    ["Range", rangePct != null ? `${rangePct.toFixed(2)}%` : "—"],
    ["Points", formatInt(data.length), true]
  ];

  return (
    <ModalShell title={`Price history · ${label || shortId(itemId)}`} onClose={onClose} wide>
      <div className="pro-surface">
        <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3 border-b border-[#16202b] px-4 pb-3.5 pt-4 sm:px-5">
          <div className="min-w-0">
            <p className="truncate font-mono text-[10px] uppercase tracking-[0.16em] text-[#5d6b7f]">
              {label || shortId(itemId)} · {periodLabel}/{frame.label}
            </p>
            <div className="mt-1.5 flex items-end gap-2">
              <span className="font-mono text-[30px] leading-none tabular-nums" style={{ color }}>
                {last ? fixedPrice(last.p) : "—"}
              </span>
              <span className="pb-0.5 font-mono text-[10px] uppercase tracking-[0.1em] text-[#5d6b7f]">
                RLT
              </span>
              {inZone ? (
                <span className="mb-0.5 border border-[#2ee6a8]/40 bg-[#2ee6a8]/10 px-1 py-0.5 font-mono text-[9px] uppercase tracking-[0.12em] text-[#2ee6a8]">
                  Buy zone
                </span>
              ) : null}
            </div>
            <p
              className="mt-1 font-mono text-[12px] tabular-nums"
              style={{ color: (change ?? 0) >= 0 ? PRO.up : PRO.down }}
            >
              {change == null ? "No change yet" : `${change >= 0 ? "▲" : "▼"} ${Math.abs(change).toFixed(2)}%`}
              <span className="text-[#5d6b7f]">{`  ·  ${periodLabel} window`}</span>
            </p>
          </div>

          <div className="shrink-0 text-right font-mono">
            <p className="text-[9px] uppercase tracking-[0.16em] text-[#5d6b7f]">Target</p>
            <p className="mt-1 text-[17px] leading-none tabular-nums" style={{ color: PRO.target }}>
              {fixedPrice(targetPrice)}
              <span className="ml-1 text-[10px] text-[#5d6b7f]">RLT</span>
            </p>
            <p className="mt-1 text-[10px] leading-snug text-[#6f8199]">
              {gapPct == null
                ? "No target set"
                : inZone
                  ? "Market at or below target"
                  : `${Math.abs(gapPct).toFixed(1)}% above target`}
            </p>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-px border-b border-[#16202b] bg-[#16202b] sm:grid-cols-5">
          {metrics.map(([name, value, hideOnMobile]) => (
            <div
              key={name}
              className={`bg-[#080c12] px-4 py-2${hideOnMobile ? " max-sm:hidden" : ""}`}
            >
              <p className="font-mono text-[9px] uppercase tracking-[0.14em] text-[#5d6b7f]">{name}</p>
              <p className="mt-0.5 truncate font-mono text-[12px] tabular-nums text-[#c6d1e0]">{value}</p>
            </div>
          ))}
        </div>

        <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 py-2.5 sm:px-5">
          <ProTabs label="Period" options={PRICE_PERIODS} value={period} onChange={onPeriod} />
          <ProTabs
            label="Bucket"
            options={Object.entries(PRICE_BUCKETS).map(([key, config]) => ({
              key,
              label: config.label
            }))}
            value={bucket}
            onChange={onBucket}
          />
        </div>

        <div className="px-1 pb-2 sm:px-2">
          <div
            className="relative h-[260px] sm:h-[360px]"
            role="img"
            aria-label={`Price history for ${label || shortId(itemId)} over ${periodLabel} in ${frame.label} closes${
              last ? `, last ${fixedPrice(last.p)} RLT` : ""
            }${change != null ? `, ${change >= 0 ? "up" : "down"} ${Math.abs(change).toFixed(1)} percent` : ""}${
              high != null && low != null ? `, high ${fixedPrice(high)}, low ${fixedPrice(low)}` : ""
            }`}
          >
            {overlay ? (
              <div className="absolute inset-0 z-10 flex items-center justify-center bg-[#080c12]/85 px-4 text-center font-mono text-[11px] uppercase tracking-[0.12em] text-[#6f8199]">
                {overlay}
              </div>
            ) : null}
            <ResponsiveContainer width="100%" height="100%">
              <ComposedChart data={chartData} margin={{ top: 10, right: 2, left: 0, bottom: 0 }}>
                <CartesianGrid vertical={false} stroke={PRO.grid} strokeDasharray="2 4" />
                <XAxis
                  dataKey="t"
                  type="number"
                  domain={xDomain}
                  tickFormatter={(value) => axisTick(value, days, frame.ms)}
                  tick={{
                    fill: PRO.axis,
                    fontSize: 10,
                    fontFamily: "JetBrains Mono, monospace",
                    letterSpacing: "0.04em"
                  }}
                  tickLine={false}
                  axisLine={{ stroke: PRO.grid }}
                  minTickGap={24}
                  ticks={ticks}
                  padding={{ left: 0, right: 0 }}
                />
                <YAxis
                  orientation="right"
                  domain={domain}
                  tickCount={5}
                  width={58}
                  tickFormatter={trimPrice}
                  tick={{
                    fill: PRO.axis,
                    fontSize: 10,
                    fontFamily: "JetBrains Mono, monospace"
                  }}
                  tickLine={false}
                  axisLine={false}
                />
                <Tooltip
                  content={
                    <ProTooltip
                      previousClose={previousClose}
                      bucketMs={frame.ms}
                      targetPrice={targetPrice}
                    />
                  }
                  cursor={{ stroke: PRO.crosshair, strokeDasharray: "3 4", strokeWidth: 1 }}
                  wrapperStyle={{ outline: "none" }}
                />
                {showTarget ? (
                  <ReferenceLine
                    y={targetPrice}
                    stroke={PRO.target}
                    strokeDasharray="4 4"
                    strokeWidth={1}
                  />
                ) : null}
                {hasBand ? (
                  <Area
                    type="linear"
                    dataKey={(point) =>
                      Number.isFinite(point?.l) && Number.isFinite(point?.h)
                        ? [point.l, point.h]
                        : null
                    }
                    stroke="none"
                    fill={color}
                    fillOpacity={0.16}
                    isAnimationActive={false}
                    connectNulls={false}
                    activeDot={false}
                  />
                ) : null}
                <Line
                  type="linear"
                  dataKey="p"
                  stroke={color}
                  strokeWidth={2}
                  isAnimationActive={false}
                  dot={false}
                  connectNulls={false}
                  activeDot={{ r: 4, fill: color, stroke: "#080c12", strokeWidth: 2 }}
                />
                {last ? (
                  <ReferenceDot
                    x={last.t}
                    y={last.p}
                    r={3}
                    fill={color}
                    stroke="#080c12"
                    strokeWidth={2}
                    ifOverflow="extendDomain"
                  />
                ) : null}
              </ComposedChart>
            </ResponsiveContainer>
          </div>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 border-t border-[#16202b] px-4 py-2 font-mono text-[9px] uppercase tracking-[0.12em] text-[#5d6b7f] sm:px-5">
          <span>
            {formatInt(data.length)} closes · {frame.label} buckets · local time
          </span>
          {hasBand ? (
            <span className="flex items-center gap-1.5">
              <i
                className="h-2.5 w-4 rounded-[1px]"
                style={{ backgroundColor: color, opacity: 0.22 }}
                aria-hidden="true"
              />
              Low–high band
            </span>
          ) : null}
          {Number.isFinite(targetPrice) ? (
            <span className="flex items-center gap-1.5">
              <i
                className="h-px w-4 border-t border-dashed border-[#f5b544]"
                aria-hidden="true"
              />
              Target {fixedPrice(targetPrice)}
            </span>
          ) : null}
          <span className={status === "stale" ? "text-[#f5b544]" : undefined}>
            {status === "stale"
              ? `Stale · ${error || "retrying"}`
              : status === "loading" && data.length > 0
                ? "Loading stored history…"
                : "Source: live market feed"}
          </span>
        </div>
      </div>
    </ModalShell>
  );
}

/** The card view is fixed and glanceable; ranges live in the detail dialog. */
const INLINE_PERIOD = "1";
const INLINE_BUCKET = "30m";

export default function PriceChart({ history, targetPrice, itemId, request, label }) {
  const [period, setPeriod] = useState(INLINE_PERIOD);
  const [bucket, setBucket] = useState(INLINE_BUCKET);
  const [stored, setStored] = useState([]);
  const [storedDay, setStoredDay] = useState([]);
  const [status, setStatus] = useState("live");
  const [error, setError] = useState(null);
  const [attempt, setAttempt] = useState(0);
  const [open, setOpen] = useState(false);
  const backfilled = useRef(false);

  const days = PRICE_PERIODS.find((option) => option.key === period)?.days ?? 1;
  const frame = PRICE_BUCKETS[bucket] ?? PRICE_BUCKETS[INLINE_BUCKET];
  const periodLabel = PRICE_PERIODS.find((option) => option.key === period)?.label ?? "1D";

  const inlineFrame = PRICE_BUCKETS[INLINE_BUCKET];
  const inlinePeriodLabel =
    PRICE_PERIODS.find((option) => option.key === INLINE_PERIOD)?.label ?? "1D";

  // Every period, 1D included, can be repaired from SQLite: the live window is
  // only 24h of in-memory closes, while the database keeps the retained week.
  useEffect(() => {
    if (!open) return undefined;
    if (!request || !itemId) {
      setStored([]);
      setError(null);
      setStatus("live");
      return undefined;
    }

    let alive = true;
    setStatus("loading");
    const load = async () => {
      try {
        const data = await request(
          `/api/price-history?itemId=${encodeURIComponent(itemId)}&days=${days}`
        );
        if (!alive) return;
        setStored(Array.isArray(data?.points) ? data.points : []);
        setError(null);
        setStatus("ready");
      } catch (caught) {
        if (!alive) return;
        setError(caught?.message || "Could not load price history.");
        setStatus("stale");
      }
    };
    load();
    return () => {
      alive = false;
    };
  }, [attempt, days, itemId, open, request]);

  // A failed long-range fetch must not strand the dialog on the live day. Try
  // again a minute later; success clears `stale` and stops the timer.
  useEffect(() => {
    if (!open || status !== "stale") return undefined;
    const timer = setTimeout(() => setAttempt((value) => value + 1), 60_000);
    return () => clearTimeout(timer);
  }, [open, status]);

  // The card needs no request of its own: the snapshot already hydrates its
  // one-day window from SQLite, and the worker pushes a sampled close every
  // minute an item is quiet. Only a card that starts with an empty window
  // (target re-added while the worker was down) asks the API to repair it.
  useEffect(() => {
    if (backfilled.current || !request || !itemId) return undefined;
    if (Array.isArray(history) && history.length > 0) {
      backfilled.current = true;
      return undefined;
    }
    backfilled.current = true;
    let alive = true;
    request(`/api/price-history?itemId=${encodeURIComponent(itemId)}&days=1`)
      .then((data) => {
        if (alive) setStoredDay(Array.isArray(data?.points) ? data.points : []);
      })
      .catch(() => {
        if (alive) setStoredDay([]);
      });
    return () => {
      alive = false;
    };
  }, [history, itemId, request]);

  const inlineData = useMemo(() => {
    const source = storedDay.length ? mergePriceSeries(storedDay, history || []) : history || [];
    return bucketPrice(source, { days: 1, bucketMs: inlineFrame.ms });
  }, [history, storedDay, inlineFrame.ms]);
  const inlineSeries = useMemo(
    () => breakPriceGaps(inlineData, inlineFrame.ms),
    [inlineData, inlineFrame.ms]
  );
  const { last, change, high, low } = useMemo(() => summarizePrice(inlineData), [inlineData]);
  const hot = Number.isFinite(last?.p) && Number.isFinite(targetPrice) && last.p <= targetPrice;
  const color = hot ? "#78f07a" : "#45e0ff";
  const { domain, showTarget } = targetDomain(
    inlineData.map((point) => point.p),
    targetPrice
  );
  const inlineXDomain =
    inlineData.length > 1
      ? ["dataMin", "dataMax"]
      : inlineData.length === 1
        ? [inlineData[0].t - inlineFrame.ms / 2, inlineData[0].t + inlineFrame.ms / 2]
        : ["dataMin", "dataMax"];

  const merged = useMemo(
    () => (stored.length ? mergePriceSeries(stored, history || []) : history || []),
    [stored, history]
  );
  const data = useMemo(
    () => bucketPrice(merged, { days, bucketMs: frame.ms }),
    [merged, days, frame.ms]
  );

  let overlay = null;
  if (data.length === 0) {
    if (status === "loading") overlay = "Loading price history…";
    else if (status === "stale") overlay = error || "Could not load price history.";
    else overlay = "Collecting data";
  }

  return (
    <>
      <div className="border-2 border-line bg-night-950/60 p-2.5">
        <div className="flex flex-wrap items-start justify-between gap-x-2 gap-y-1">
          <div className="flex items-baseline gap-2 whitespace-nowrap font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
            Price history
            <span
              className={`tabular-nums ${
                change === null ? "text-faint" : change >= 0 ? "text-signal" : "text-danger"
              }`}
              title="The card view is fixed at 1 day with 30-minute buckets. Open the chart for 1D–7D and other buckets."
            >
              {change !== null ? `${change >= 0 ? "+" : ""}${change.toFixed(1)}% · ` : ""}
              {inlinePeriodLabel}/{inlineFrame.label}
            </span>
          </div>
          <span
            className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint tabular-nums"
            title="High and low across this window, including intra-bucket wicks"
          >
            H {fixedPrice(high)} · L {fixedPrice(low)}
          </span>
        </div>

        <button
          type="button"
          className="chart-expand"
          aria-haspopup="dialog"
          aria-label={`Open detailed price history for ${label || shortId(itemId)}`}
          onClick={() => setOpen(true)}
        >
          <span className="chart-expand-badge" aria-hidden="true">
            <ExpandIcon />
            <span className="chart-expand-label">Expand</span>
          </span>
          <div
            className="relative h-[112px]"
            role="img"
            aria-label={`Price history as a line over ${inlinePeriodLabel} in ${inlineFrame.label} buckets${
              change !== null ? `, ${change >= 0 ? "up" : "down"} ${Math.abs(change).toFixed(1)} percent` : ""
            }`}
          >
            {inlineData.length === 0 ? (
              <div className="absolute inset-0 z-10 flex items-center justify-center bg-night-950/60 px-3 text-center font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
                Collecting data
              </div>
            ) : null}
            <ResponsiveContainer width="100%" height="100%">
              {/* Line, not bars: 48 fixed 30-minute points would render as
                  two-pixel slivers in the card. */}
              <LineChart
                data={inlineSeries}
                margin={{ top: 8, right: 38, left: 0, bottom: 0 }}
                accessibilityLayer={false}
              >
                <XAxis
                  dataKey="t"
                  type="number"
                  scale="time"
                  domain={inlineXDomain}
                  tickFormatter={(value) => axisTick(value, 1, inlineFrame.ms)}
                  tick={{ fill: "#8576c4", fontSize: 12, fontFamily: "JetBrains Mono, monospace", letterSpacing: "0.02em" }}
                  tickLine={false}
                  axisLine={false}
                  minTickGap={26}
                  padding={{ left: 0, right: 0 }}
                />
                <YAxis hide domain={domain} />
                <Tooltip
                  content={<ChartTooltip bucketMs={inlineFrame.ms} />}
                  cursor={{ stroke: color, strokeOpacity: 0.35, strokeDasharray: "3 4" }}
                  wrapperStyle={{ outline: "none" }}
                />
                {showTarget ? (
                  <ReferenceLine
                    y={targetPrice}
                    stroke="#ffd23f"
                    strokeDasharray="3 4"
                    strokeOpacity={0.75}
                    ifOverflow="extendDomain"
                  />
                ) : null}
                <Line
                  type="linear"
                  dataKey="p"
                  stroke={color}
                  strokeWidth={1.6}
                  dot={false}
                  activeDot={{ r: 3, fill: color, stroke: "#080617", strokeWidth: 2 }}
                  isAnimationActive={false}
                  connectNulls={false}
                />
              </LineChart>
            </ResponsiveContainer>
          </div>
        </button>
      </div>

      {open ? (
        <PriceDetailModal
          label={label}
          itemId={itemId}
          data={data}
          period={period}
          bucket={bucket}
          days={days}
          frame={frame}
          periodLabel={periodLabel}
          targetPrice={targetPrice}
          status={status}
          error={error}
          overlay={overlay}
          onPeriod={setPeriod}
          onBucket={setBucket}
          onClose={() => setOpen(false)}
        />
      ) : null}
    </>
  );
}
