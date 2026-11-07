import { useEffect, useId, useMemo, useState } from "react";
import {
  Bar,
  BarChart,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import { fixedPrice, formatTime } from "../lib/format.js";
import {
  bucketPrice,
  mergePriceSeries,
  PRICE_BUCKETS,
  PRICE_DEFAULT_BUCKET,
  PRICE_DEFAULT_PERIOD,
  PRICE_PERIODS,
  summarizePrice
} from "../lib/price.js";

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

function Segmented({ label, options, value, onChange }) {
  return (
    <div className="flex items-center gap-1">
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
                ? "bg-gold font-bold text-[#1a1000]"
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

export default function PriceChart({ history, targetPrice, itemId, request }) {
  const [period, setPeriod] = useState(PRICE_DEFAULT_PERIOD);
  const [bucket, setBucket] = useState(PRICE_DEFAULT_BUCKET);
  const [stored, setStored] = useState([]);
  const [status, setStatus] = useState("live");
  const [error, setError] = useState(null);
  const [attempt, setAttempt] = useState(0);
  const gradientId = useId().replace(/[^a-zA-Z0-9_-]/g, "");

  const days = PRICE_PERIODS.find((option) => option.key === period)?.days ?? 1;
  const frame = PRICE_BUCKETS[bucket] ?? PRICE_BUCKETS[PRICE_DEFAULT_BUCKET];

  useEffect(() => {
    if (days <= 1 || !request || !itemId) {
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
  }, [attempt, days, itemId, request]);

  // A failed long-range fetch must not strand the chart on the live day. Try
  // again a minute later; success clears `stale` and stops the timer.
  useEffect(() => {
    if (status !== "stale") return undefined;
    const timer = setTimeout(() => setAttempt((value) => value + 1), 60_000);
    return () => clearTimeout(timer);
  }, [status]);

  const merged = useMemo(
    () => (days <= 1 ? history || [] : mergePriceSeries(stored, history)),
    [days, stored, history]
  );
  const data = useMemo(
    () => bucketPrice(merged, { days, bucketMs: frame.ms }),
    [merged, days, frame.ms]
  );
  const { last, change, high, low } = useMemo(() => summarizePrice(data), [data]);
  const hot = Number.isFinite(last?.p) && Number.isFinite(targetPrice) && last.p <= targetPrice;
  const color = hot ? "#78f07a" : "#45e0ff";

  const values = data.map((point) => point.p);
  if (Number.isFinite(targetPrice)) values.push(targetPrice);
  let min = values.length ? Math.min(...values) : 0;
  let max = values.length ? Math.max(...values) : 1;
  if (!(max > min)) {
    const padding = Math.max(min * 0.01, 0.000001);
    min -= padding;
    max += padding;
  }
  const domain = [min - (max - min) * 0.12, max + (max - min) * 0.12];
  const xDomain =
    data.length > 1
      ? ["dataMin", "dataMax"]
      : data.length === 1
        ? [data[0].t - frame.ms / 2, data[0].t + frame.ms / 2]
        : ["dataMin", "dataMax"];

  let overlay = null;
  if (data.length === 0) {
    if (status === "loading") overlay = "Loading price history…";
    else if (status === "stale") overlay = error || "Could not load price history.";
    else overlay = "Collecting data";
  }

  const periodLabel = PRICE_PERIODS.find((option) => option.key === period)?.label ?? "1D";

  return (
    <div className="border-2 border-line bg-night-950/60 p-2.5">
      <div className="flex flex-wrap items-start justify-between gap-x-2 gap-y-1">
        <div className="flex items-baseline gap-2 whitespace-nowrap font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          Price history
          {change !== null && (
            <span className={`tabular-nums ${change >= 0 ? "text-signal" : "text-danger"}`}>
              {change >= 0 ? "+" : ""}
              {change.toFixed(1)}% · {periodLabel}/{frame.label}
            </span>
          )}
          {status === "stale" ? (
            <span className="text-warn" title={error || "Retrying"}>
              Stale
            </span>
          ) : null}
        </div>
        <span
          className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint tabular-nums"
          title="High and low closes in this window"
        >
          H {fixedPrice(high)} · L {fixedPrice(low)}
        </span>
      </div>

      <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
        <Segmented
          label="Period"
          value={period}
          onChange={setPeriod}
          options={PRICE_PERIODS}
        />
        <Segmented
          label="Bucket"
          value={bucket}
          onChange={setBucket}
          options={Object.entries(PRICE_BUCKETS).map(([key, config]) => ({
            key,
            label: config.label
          }))}
        />
      </div>

      <div
        className="relative mt-1.5 h-[112px]"
        role="img"
        aria-label={`Price history as bars over ${periodLabel} in ${frame.label} buckets${
          change !== null ? `, ${change >= 0 ? "up" : "down"} ${Math.abs(change).toFixed(1)} percent` : ""
        }`}
      >
        {overlay ? (
          <div className="absolute inset-0 z-10 flex items-center justify-center bg-night-950/60 px-3 text-center font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
            {overlay}
          </div>
        ) : null}
        <ResponsiveContainer width="100%" height="100%">
          {/* Bars grow from the bottom of the visible price domain instead of
              zero, so a 0.017–0.019 window still shows its shape: Recharts
              picks that baseline automatically when the domain stays positive. */}
          <BarChart
            data={data}
            margin={{ top: 8, right: 38, left: 0, bottom: 0 }}
            barCategoryGap={0}
          >
            <defs>
              <linearGradient id={`bars-${gradientId}`} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={color} stopOpacity={0.95} />
                <stop offset="100%" stopColor={color} stopOpacity={0.35} />
              </linearGradient>
            </defs>
            <XAxis
              dataKey="t"
              type="number"
              scale="time"
              domain={xDomain}
              tickFormatter={(value) => axisTick(value, days, frame.ms)}
              tick={{ fill: "#8576c4", fontSize: 12, fontFamily: "JetBrains Mono, monospace", letterSpacing: "0.02em" }}
              tickLine={false}
              axisLine={false}
              minTickGap={26}
              padding={{ left: 0, right: 0 }}
            />
            <YAxis hide domain={domain} />
            <Tooltip
              content={<ChartTooltip bucketMs={frame.ms} />}
              cursor={{ fill: hot ? "rgba(120, 240, 122, 0.08)" : "rgba(69, 224, 255, 0.08)" }}
              wrapperStyle={{ outline: "none" }}
            />
            {Number.isFinite(targetPrice) ? (
              <ReferenceLine
                y={targetPrice}
                stroke="#ffd23f"
                strokeDasharray="3 4"
                strokeOpacity={0.75}
                ifOverflow="extendDomain"
              />
            ) : null}
            <Bar
              dataKey="p"
              fill={`url(#bars-${gradientId})`}
              maxBarSize={data.length > 240 ? 3 : 18}
              isAnimationActive={false}
            />
          </BarChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
