import { useEffect, useMemo, useState } from "react";
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

import { formatInt, formatPreciseMs, formatTime, relTime } from "../lib/format.js";
import { timezoneOffsetMs } from "../lib/price.js";
import {
  jitterBand,
  latencyDomain,
  orderAxisTick,
  orderBucket,
  orderHistoryPath,
  orderPeriod,
  orderRangeLabel,
  orderStage,
  orderTimeTicks,
  ORDER_BUCKETS,
  ORDER_PERIODS,
  ORDER_STAGES
} from "../lib/order-chart.js";

/**
 * Order-latency charts, in the same quiet terminal style as the expanded price
 * history. The compact view plots individual orders; the expanded view
 * aggregates the loot log into local-clock buckets so avg/p95/jitter can be
 * read over hours or days without a fixed 200-order ceiling.
 */
const PRO = Object.freeze({
  avg: "#45e0ff",
  p95: "#f5b544",
  band: "#45e0ff",
  down: "#ff5d73",
  up: "#2ee6a8",
  grid: "#16202b",
  axis: "#5d6b7f",
  crosshair: "#3d4d61"
});

function pad(value) {
  return String(value).padStart(2, "0");
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

function axisMs(value) {
  const number = Number(value);
  if (!Number.isFinite(number) || number < 0) return "—";
  if (number < 1000) return `${Math.round(number)}ms`;
  if (number < 10_000) return `${(number / 1000).toFixed(2)}s`;
  return `${(number / 1000).toFixed(1)}s`;
}

function previewTick(value, span) {
  const date = new Date(value);
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (span <= 15 * 60_000) return `${clock}:${pad(date.getSeconds())}`;
  if (span <= 24 * 60 * 60_000) return clock;
  return orderAxisTick(value, span, 5 * 60_000);
}

function PointTooltip({ active, payload }) {
  if (!active || !payload?.length) return null;
  const point = payload[0]?.payload;
  if (!Number.isFinite(point?.total)) return null;
  return (
    <div className="pro-tip">
      <p className="pro-tip-range">{formatTime(point.at)}</p>
      <div className="pro-tip-row">
        <span>Signal → done</span>
        <strong>{formatPreciseMs(point.total)}</strong>
      </div>
      <div className="pro-tip-row">
        <span>Status</span>
        <b style={{ color: point.status === "failed" ? PRO.down : PRO.up }}>
          {point.status === "failed" ? "FAILED" : "FILLED"}
        </b>
      </div>
    </div>
  );
}

function BucketTooltip({ active, payload, bucketMs }) {
  if (!active || !payload?.length) return null;
  const point = payload[0]?.payload;
  if (!Number.isFinite(point?.at)) return null;
  return (
    <div className="pro-tip">
      <p className="pro-tip-range">{orderRangeLabel(point.at, bucketMs)}</p>
      <div className="pro-tip-row">
        <span>Avg</span>
        <strong>{formatPreciseMs(point.avg)}</strong>
      </div>
      <div className="pro-tip-row">
        <span>P95</span>
        <b style={{ color: PRO.p95 }}>{formatPreciseMs(point.p95)}</b>
      </div>
      <div className="pro-tip-row">
        <span>Jitter</span>
        <b>{formatPreciseMs(point.jitter)}</b>
      </div>
      <div className="pro-tip-row">
        <span>Orders</span>
        <strong>{formatInt(point.n)}</strong>
      </div>
      {point.failed > 0 ? (
        <div className="pro-tip-row">
          <span>Failed</span>
          <b style={{ color: PRO.down }}>{formatInt(point.failed)}</b>
        </div>
      ) : null}
    </div>
  );
}

/** Compact chart on the benchmark panel: one point per order, newest last. */
function Preview({ series, p95, onExpand }) {
  const points = Array.isArray(series) ? series : [];
  const last = points[points.length - 1] || null;
  const failed = points.filter((point) => point.status === "failed");
  const domainValues = points
    .map((point) => Number(point.total))
    .filter((value) => Number.isFinite(value));
  if (Number.isFinite(Number(p95))) domainValues.push(Number(p95));
  const domain = latencyDomain(domainValues);
  const span =
    points.length > 1 ? points[points.length - 1].at - points[0].at : 60_000;
  const ticks =
    points.length > 1
      ? orderTimeTicks(points[0].at, points[points.length - 1].at, timezoneOffsetMs())
      : undefined;
  const xDomain =
    points.length > 1
      ? ["dataMin", "dataMax"]
      : points.length === 1
        ? [points[0].at - 30_000, points[0].at + 30_000]
        : ["dataMin", "dataMax"];

  return (
    <div className="border-2 border-line bg-night-950/60 p-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-x-2 gap-y-1">
        <span className="flex items-center gap-1.5 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span className="led bg-info" aria-hidden="true" /> Order latency
        </span>
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] tabular-nums text-faint">
          {last ? `${formatPreciseMs(last.total)} last` : "no samples"}
          {Number.isFinite(Number(p95)) ? ` · P95 ${formatPreciseMs(p95)}` : ""}
        </span>
      </div>

      <button
        className="chart-expand"
        type="button"
        aria-haspopup="dialog"
        aria-label="Open the expanded order latency chart"
        onClick={onExpand}
      >
        <span className="chart-expand-badge" aria-hidden="true">
          <ExpandIcon />
          <span className="chart-expand-label">Expand</span>
        </span>
        <div className="relative h-[116px]" role="img" aria-label="Order latency for the most recent orders">
          {points.length === 0 ? (
            <div className="absolute inset-0 z-10 flex items-center justify-center bg-night-950/60 px-3 text-center font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
              Collecting order timings
            </div>
          ) : null}
          <ResponsiveContainer width="100%" height="100%">
            <LineChart data={points} margin={{ top: 8, right: 16, left: 0, bottom: 0 }} accessibilityLayer={false}>
              <XAxis
                dataKey="at"
                type="number"
                scale="time"
                domain={xDomain}
                tickFormatter={(value) => previewTick(value, span)}
                tick={{ fill: "#8576c4", fontSize: 11, fontFamily: "JetBrains Mono, monospace", letterSpacing: "0.02em" }}
                tickLine={false}
                axisLine={false}
                minTickGap={28}
                ticks={ticks}
                padding={{ left: 0, right: 0 }}
              />
              <YAxis hide domain={domain} />
              <Tooltip
                content={<PointTooltip />}
                cursor={{ stroke: PRO.crosshair, strokeDasharray: "3 4", strokeWidth: 1 }}
                wrapperStyle={{ outline: "none" }}
              />
              {Number.isFinite(Number(p95)) ? (
                <ReferenceLine
                  y={Number(p95)}
                  stroke={PRO.p95}
                  strokeDasharray="3 4"
                  strokeOpacity={0.65}
                  ifOverflow="extendDomain"
                />
              ) : null}
              <Line
                type="linear"
                dataKey="total"
                stroke={PRO.avg}
                strokeWidth={1.6}
                dot={
                  points.length <= 2
                    ? { r: 3, fill: PRO.avg, stroke: "#080617", strokeWidth: 2 }
                    : false
                }
                activeDot={{ r: 3.5, fill: PRO.avg, stroke: "#080617", strokeWidth: 2 }}
                isAnimationActive={false}
                connectNulls={false}
              />
              {failed.map((point, index) => (
                <ReferenceDot
                  key={`${point.at}-${index}`}
                  x={point.at}
                  y={point.total}
                  r={2.5}
                  fill={PRO.down}
                  stroke="#080617"
                  strokeWidth={1.5}
                  ifOverflow="extendDomain"
                />
              ))}
            </LineChart>
          </ResponsiveContainer>
        </div>
      </button>

      <div className="mt-1 flex flex-wrap items-center justify-between gap-x-3 gap-y-1 border-t border-line/60 pt-1.5 font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
        <span>{formatInt(points.length)} recent orders</span>
        <span>{formatInt(failed.length)} failed</span>
      </div>
    </div>
  );
}

function useOrderHistory({ request, hours, bucket, stage }) {
  const [history, setHistory] = useState(null);
  const [status, setStatus] = useState("loading");
  const [error, setError] = useState(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (typeof request !== "function") return undefined;
    let alive = true;
    const load = async () => {
      try {
        const payload = await request(
          orderHistoryPath({
            hours,
            bucket,
            stage,
            tzOffsetMs: timezoneOffsetMs()
          })
        );
        if (!alive) return;
        setHistory(payload?.history || null);
        setError(null);
        setStatus("ready");
      } catch (caught) {
        if (!alive) return;
        setError(caught?.message || "Could not load order history.");
        setStatus("stale");
      }
    };
    load();
    const timer = setInterval(load, 30_000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [attempt, bucket, hours, request, stage]);

  // Keep a failed window open but try again quietly; the chart stays on the
  // last good payload instead of collapsing to an error card.
  useEffect(() => {
    if (status !== "stale") return undefined;
    const timer = setTimeout(() => setAttempt((value) => value + 1), 30_000);
    return () => clearTimeout(timer);
  }, [status]);

  return { history, status, error };
}

/** Expanded view: period/bucket/stage controls and a full-height plot. */
function Detail({ request, benchmark, onCollapse }) {
  const [periodKey, setPeriodKey] = useState("24h");
  const [bucketKey, setBucketKey] = useState("1h");
  const [stageKey, setStageKey] = useState("total");
  const period = orderPeriod(periodKey);
  const frame = orderBucket(bucketKey);
  const stage = orderStage(stageKey);
  const { history, status, error } = useOrderHistory({
    request,
    hours: period.hours,
    bucket: bucketKey,
    stage: stageKey
  });

  const points = history?.points || [];
  const summary = history?.summary || {};
  // Headline = newest bucket only. The whole-range summary never moves when
  // the bucket flips, which reads as broken; the newest bucket's stats do
  // move, because a different bucket size means a different slice.
  const bucketSummary = history?.bucketSummary || {};
  const headline = {
    avg: bucketSummary.avg,
    p95: bucketSummary.p95,
    jitter: bucketSummary.jitter,
    orders: bucketSummary.n || 0
  };
  const chartData = useMemo(
    () => points.map((point) => ({ ...point, band: jitterBand(point) })),
    [points]
  );
  const domain = useMemo(
    () =>
      latencyDomain(
        points.flatMap((point) => [
          Number(point.avg),
          Number(point.p95),
          ...(jitterBand(point) || [])
        ])
      ),
    [points]
  );
  // The first bucket can start just before `since` (a 1h bucket aligned at
  // 19:00 for a 19:45 window). Start the axis at that bucket instead of
  // clipping the first point outside the domain.
  const rangeStart = history
    ? points.length
      ? Math.min(history.since, points[0].at)
      : history.since
    : Date.now() - 24 * 60 * 60_000;
  const span = history ? Math.max(1, history.until - rangeStart) : 24 * 60 * 60_000;
  const ticks =
    points.length > 1 && history
      ? orderTimeTicks(rangeStart, history.until, history.tzOffsetMs)
      : undefined;
  const xDomain = history
    ? points.length === 1
      ? [points[0].at - frame.ms / 2, points[0].at + frame.ms / 2]
      : [rangeStart, history.until]
    : ["dataMin", "dataMax"];
  const lastOrder = Number(benchmark?.lastOrderAt) || null;
  const successRate =
    summary.orders > 0 ? (summary.filled / summary.orders) * 100 : null;

  const changePeriod = (key) => {
    setPeriodKey(key);
    setBucketKey(orderPeriod(key).bucket);
  };

  const metrics = [
    ["Orders", formatInt(summary.orders || 0)],
    ["Filled", formatInt(summary.filled || 0), true],
    ["Failed", formatInt(summary.failed || 0)],
    ["Success", successRate == null ? "—" : `${successRate.toFixed(1)}%`],
    ["Buckets", formatInt(points.length), true]
  ];

  let overlay = null;
  if (points.length === 0) {
    if (status === "loading") overlay = "Loading order history…";
    else if (status === "stale") overlay = error || "Could not load order history.";
    else overlay = "No timed orders in this window";
  }

  return (
    <div className="pro-surface flex min-h-full flex-col">
      <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3 border-b border-[#16202b] px-4 pb-3.5 pt-4 sm:px-5">
        <div className="min-w-0">
          <p className="truncate font-mono text-[10px] uppercase tracking-[0.16em] text-[#5d6b7f]">
            Order latency · {period.label}/{frame.label} · {stage.label}
          </p>
          <div className="mt-1.5 flex items-end gap-2">
            <span className="font-mono text-[30px] leading-none tabular-nums" style={{ color: PRO.avg }}>
              {headline.avg == null ? "—" : formatPreciseMs(headline.avg)}
            </span>
            <span className="pb-0.5 font-mono text-[10px] uppercase tracking-[0.1em] text-[#5d6b7f]">
              avg
            </span>
          </div>
          <p className="mt-1 font-mono text-[12px] tabular-nums text-[#8fa0b8]">
            P95 <b style={{ color: PRO.p95 }}>{formatPreciseMs(headline.p95)}</b>
            <span className="text-[#5d6b7f]"> · </span>
            Jitter <b className="text-[#c6d1e0]">{formatPreciseMs(headline.jitter)}</b>
            <span className="text-[#5d6b7f]"> · {formatInt(headline.orders || 0)} orders in bucket</span>
          </p>
          <p className="mt-0.5 font-mono text-[10px] uppercase tracking-[0.08em] text-[#5d6b7f]">
            Newest bucket · whole window avg {summary.avg == null ? "—" : formatPreciseMs(summary.avg)}
          </p>
        </div>

        <div className="flex shrink-0 flex-col items-end gap-2 font-mono">
          <button
            type="button"
            className="border border-[#223040] px-2 py-1 text-[10px] uppercase tracking-[0.1em] text-[#8fa0b8] transition-colors hover:bg-[#121b26] hover:text-[#e8eef7]"
            onClick={() => onCollapse(false)}
          >
            ← Back to monitor
          </button>
          <p className="text-[10px] leading-snug text-[#6f8199]">
            {lastOrder ? `Last order ${relTime(lastOrder)}` : "Waiting for the first order"}
          </p>
        </div>
      </div>

      <div className="grid grid-cols-3 gap-px border-b border-[#16202b] bg-[#16202b] sm:grid-cols-5">
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
        <ProTabs
          label="Period"
          options={ORDER_PERIODS.map(({ key, label }) => ({ key, label }))}
          value={periodKey}
          onChange={changePeriod}
        />
        <ProTabs
          label="Bucket"
          options={ORDER_BUCKETS.map(({ key, label }) => ({ key, label }))}
          value={bucketKey}
          onChange={setBucketKey}
        />
        <ProTabs
          label="Stage"
          options={ORDER_STAGES.map(({ key, label }) => ({ key, label }))}
          value={stageKey}
          onChange={setStageKey}
        />
      </div>

      <div className="flex flex-col px-1 pb-2 sm:px-2">
        <div
          className="relative h-[min(62vh,560px)] min-h-[320px]"
          role="img"
          aria-label={`Order ${stage.label} latency over ${period.label} in ${frame.label} buckets${
            headline.avg != null ? `, newest bucket average ${formatPreciseMs(headline.avg)}` : ""
          }${headline.p95 != null ? `, p95 ${formatPreciseMs(headline.p95)}` : ""}`}
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
                dataKey="at"
                type="number"
                domain={xDomain}
                tickFormatter={(value) => orderAxisTick(value, span, frame.ms)}
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
                width={54}
                tickFormatter={axisMs}
                tick={{ fill: PRO.axis, fontSize: 10, fontFamily: "JetBrains Mono, monospace" }}
                tickLine={false}
                axisLine={false}
              />
              <Tooltip
                content={<BucketTooltip bucketMs={frame.ms} />}
                cursor={{ stroke: PRO.crosshair, strokeDasharray: "3 4", strokeWidth: 1 }}
                wrapperStyle={{ outline: "none" }}
              />
              <Area
                type="linear"
                dataKey="band"
                stroke="none"
                fill={PRO.band}
                fillOpacity={0.14}
                isAnimationActive={false}
                connectNulls={false}
              />
              <Line
                type="linear"
                dataKey="avg"
                stroke={PRO.avg}
                strokeWidth={2}
                isAnimationActive={false}
                dot={
                  points.length <= 2
                    ? { r: 3.5, fill: PRO.avg, stroke: "#080c12", strokeWidth: 2 }
                    : false
                }
                connectNulls={false}
                activeDot={{ r: 4, fill: PRO.avg, stroke: "#080c12", strokeWidth: 2 }}
              />
              <Line
                type="linear"
                dataKey="p95"
                stroke={PRO.p95}
                strokeWidth={1.5}
                strokeDasharray="4 4"
                isAnimationActive={false}
                dot={
                  points.length <= 2
                    ? { r: 3, fill: PRO.p95, stroke: "#080c12", strokeWidth: 2 }
                    : false
                }
                connectNulls={false}
                activeDot={{ r: 3.5, fill: PRO.p95, stroke: "#080c12", strokeWidth: 2 }}
              />
            </ComposedChart>
          </ResponsiveContainer>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 border-t border-[#16202b] px-4 py-2 font-mono text-[9px] uppercase tracking-[0.12em] text-[#5d6b7f] sm:px-5">
        <span>
          {formatInt(points.length)} buckets · {frame.label} · local time
        </span>
        <span className="flex items-center gap-3">
          <span className="flex items-center gap-1.5">
            <i className="h-px w-4" style={{ background: PRO.avg }} aria-hidden="true" />
            Avg
          </span>
          <span className="flex items-center gap-1.5">
            <i
              className="h-px w-4 border-t border-dashed"
              style={{ borderColor: PRO.p95 }}
              aria-hidden="true"
            />
            P95
          </span>
          <span className="flex items-center gap-1.5">
            <i className="h-2 w-3" style={{ background: PRO.band, opacity: 0.3 }} aria-hidden="true" />
            Jitter
          </span>
        </span>
        <span className={status === "stale" ? "text-[#f5b544]" : undefined}>
          {status === "stale"
            ? `Stale · ${error || "retrying"}`
            : "Source: loot log timing marks"}
        </span>
      </div>
    </div>
  );
}

/** Lazy entry point. `mode` keeps both views in one Recharts chunk. */
export default function OrderBenchmarkChart({ mode = "preview", ...props }) {
  return mode === "detail" ? <Detail {...props} /> : <Preview {...props} />;
}
