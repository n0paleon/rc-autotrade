import { useId, useMemo, useState } from "react";
import {
  Area,
  AreaChart,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import { fixedPrice, formatTime } from "../lib/format.js";

const TIMEFRAMES = {
  "1m": { ms: 60_000, points: 60, label: "1m" },
  "5m": { ms: 5 * 60_000, points: 72, label: "5m" },
  "15m": { ms: 15 * 60_000, points: 96, label: "15m" },
  "1h": { ms: 60 * 60_000, points: 24, label: "1h" }
};

function aggregate(history, timeframe) {
  const config = TIMEFRAMES[timeframe];
  if (!Array.isArray(history)) return [];
  const buckets = new Map();
  for (const point of history) {
    if (!Number.isFinite(point?.t) || !Number.isFinite(point?.p)) continue;
    const bucket = Math.floor(point.t / config.ms) * config.ms;
    buckets.set(bucket, point.p);
  }
  return [...buckets.entries()]
    .map(([t, p]) => ({ t, p }))
    .sort((a, b) => a.t - b.t)
    .slice(-config.points);
}

function ChartTooltip({ active, payload, label }) {
  if (!active || !payload?.length) return null;
  return (
    <div className="chart-tip">
      <span>{formatTime(label)}</span>
      <strong>{fixedPrice(payload[0].value)} RLT</strong>
    </div>
  );
}

export default function PriceChart({ history, targetPrice }) {
  const [timeframe, setTimeframe] = useState("1m");
  const gradientId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const data = useMemo(() => aggregate(history, timeframe), [history, timeframe]);
  const last = data[data.length - 1];
  const first = data[0];
  const change =
    last && first && first.p > 0 ? ((last.p - first.p) / first.p) * 100 : null;
  const hot = Number.isFinite(last?.p) && Number.isFinite(targetPrice) && last.p <= targetPrice;
  const color = hot ? "#cde736" : "#57d1e3";

  const values = data.map((point) => point.p);
  if (Number.isFinite(targetPrice)) values.push(targetPrice);
  let min = values.length ? Math.min(...values) : 0;
  let max = values.length ? Math.max(...values) : 1;
  if (!(max > min)) {
    const pad = Math.max(min * 0.01, 0.000001);
    min -= pad;
    max += pad;
  }
  const pad = (max - min) * 0.12;
  const domain = [min - pad, max + pad];

  return (
    <div className="rounded-lg border border-line bg-carbon-950/40 p-2.5">
      <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1">
        <div className="flex items-baseline gap-2 whitespace-nowrap font-mono text-[9px] uppercase tracking-[0.14em] text-faint">
          Price history
          {change !== null && (
            <span className={`tabular-nums ${change >= 0 ? "text-signal" : "text-danger"}`}>
              {change >= 0 ? "+" : ""}
              {change.toFixed(1)}% · {TIMEFRAMES[timeframe].label}
            </span>
          )}
        </div>
        <div
          className="ml-auto -my-0.5 flex shrink-0 items-center gap-0.5 rounded border border-line bg-carbon-900 p-0.5"
          role="tablist"
          aria-label="Chart timeframe"
        >
          {Object.entries(TIMEFRAMES).map(([key, config]) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={timeframe === key}
              className={`inline-flex h-6 min-w-6 items-center justify-center rounded px-1.5 font-mono text-[8.5px] uppercase tracking-[0.08em] transition-colors ${
                timeframe === key ? "bg-carbon-800 text-signal" : "text-faint hover:text-ink-dim"
              }`}
              onClick={() => setTimeframe(key)}
            >
              {config.label}
            </button>
          ))}
        </div>
      </div>

      <div
        className="relative mt-1.5 h-[112px]"
        role="img"
        aria-label={`Price history over ${TIMEFRAMES[timeframe].label}${change !== null ? `, ${change >= 0 ? "up" : "down"} ${Math.abs(change).toFixed(1)} percent` : ""}`}
      >
        {data.length < 2 ? (
          <div className="absolute inset-0 z-10 flex items-center justify-center rounded bg-carbon-950/40 font-mono text-[9px] uppercase tracking-[0.12em] text-faint">
            {data.length === 0 ? "Collecting data" : "1 tick received"}
          </div>
        ) : null}
        <ResponsiveContainer width="100%" height="100%">
          <AreaChart data={data} margin={{ top: 8, right: 2, left: 0, bottom: 0 }}>
            <defs>
              <linearGradient id={`area-${gradientId}`} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={color} stopOpacity={0.24} />
                <stop offset="100%" stopColor={color} stopOpacity={0.015} />
              </linearGradient>
            </defs>
            <XAxis
              dataKey="t"
              type="number"
              scale="time"
              domain={["dataMin", "dataMax"]}
              tickFormatter={(value) =>
                timeframe === "1h" ? `${String(new Date(value).getHours()).padStart(2, "0")}:00` : formatTime(value).slice(0, 5)
              }
              tick={{ fill: "#7b827c", fontSize: 9, fontFamily: "JetBrains Mono, monospace" }}
              tickLine={false}
              axisLine={false}
              minTickGap={26}
              padding={{ left: 0, right: 0 }}
            />
            <YAxis hide domain={domain} />
            <Tooltip
              content={<ChartTooltip />}
              cursor={{ stroke: "rgba(147, 154, 148, 0.35)", strokeDasharray: "2 3" }}
              wrapperStyle={{ outline: "none" }}
            />
            <ReferenceLine
              y={targetPrice}
              stroke="#fac053"
              strokeDasharray="3 4"
              strokeOpacity={0.65}
              ifOverflow="extendDomain"
            />
            <Area
              type="monotone"
              dataKey="p"
              stroke={color}
              strokeWidth={1.75}
              fill={`url(#area-${gradientId})`}
              dot={false}
              activeDot={{ r: 2.5, fill: color, stroke: "#070c08", strokeWidth: 1 }}
              isAnimationActive={false}
            />
          </AreaChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
