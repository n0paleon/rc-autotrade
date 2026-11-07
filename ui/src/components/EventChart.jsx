import { useEffect, useId, useMemo, useState } from "react";
import {
  Bar,
  BarChart,
  Cell,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis
} from "recharts";
import {
  bucketEvents,
  EVENT_DEFAULT_PERIOD,
  EVENT_DEFAULT_TIMEFRAME,
  EVENT_PERIODS,
  EVENT_TIMEFRAMES,
  summarizeEvents
} from "../lib/events.js";
import { formatInt } from "../lib/format.js";
import { Icon } from "./ModalShell.jsx";

const REFRESH_MS = 60_000;

function pad(value) {
  return String(value).padStart(2, "0");
}

function clock(ts) {
  const date = new Date(ts);
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
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
  if (sameDay) return `${clock(start)}–${clock(start + bucketMs)}`;
  return `${day(start)} ${clock(start)}–${day(start + bucketMs)} ${clock(start + bucketMs)}`;
}

function axisTick(value, days) {
  const date = new Date(value);
  const hours = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (days <= 1) return hours;
  return `${date.getDate()}/${date.getMonth() + 1} ${hours}`;
}

function EventTooltip({ active, payload, bucketMs }) {
  if (!active || !payload?.length) return null;
  const point = payload[0].payload;
  const minutes = bucketMs / 60_000;
  const rate = minutes > 0 ? point.n / minutes : 0;
  return (
    <div className="chart-tip">
      <span>{rangeLabel(point.t, bucketMs)}</span>
      <strong>{formatInt(point.n)} events</strong>
      <span className="text-faint">{rate.toFixed(1)}/min</span>
    </div>
  );
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
                ? "bg-pink font-bold text-[#2a0018]"
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
 * Market-frame activity over the last 1/3/7 days.
 *
 * The worker counts every frame into 5-minute buckets; this panel re-buckets
 * those rows to the chosen timeframe and aligns the bars to the viewer's local
 * clock, which is what makes "21:00 is the busy hour" readable.
 */
export default function EventChart({ request }) {
  const [period, setPeriod] = useState(EVENT_DEFAULT_PERIOD);
  const [timeframe, setTimeframe] = useState(EVENT_DEFAULT_TIMEFRAME);
  const [points, setPoints] = useState([]);
  const [status, setStatus] = useState("loading");
  const [error, setError] = useState(null);
  const gradientId = useId().replace(/[^a-zA-Z0-9_-]/g, "");

  useEffect(() => {
    let alive = true;
    let timer = null;
    setPoints([]);
    setStatus("loading");

    const load = async () => {
      try {
        const data = await request(`/api/events?days=${period}`);
        if (!alive) return;
        setPoints(Array.isArray(data?.points) ? data.points : []);
        setError(null);
        setStatus("ready");
      } catch (caught) {
        if (!alive) return;
        setError(caught?.message || "Could not load market history.");
        // A failed poll must not blank a chart that already has history; only a
        // first load with nothing to show earns the full error state.
        setStatus((previous) => (previous === "loading" ? "error" : "stale"));
      }
    };

    load();
    timer = setInterval(load, REFRESH_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [period, request]);

  const days = EVENT_PERIODS.find((item) => item.key === period)?.days ?? 1;
  const frame = EVENT_TIMEFRAMES[timeframe] ?? EVENT_TIMEFRAMES[EVENT_DEFAULT_TIMEFRAME];
  const data = useMemo(
    () => bucketEvents(points, { days, bucketMs: frame.ms }),
    [points, days, frame.ms]
  );
  const { total, peak, average } = useMemo(() => summarizeEvents(data), [data]);
  const busiest = peak?.t ?? null;
  const yMax = Math.max(1, peak?.n ?? 0);

  let overlay = null;
  if (status === "loading") overlay = "Loading market history…";
  else if (status === "error") overlay = error || "Could not load market history.";
  else if (total === 0)
    overlay = points.length
      ? "No market events in this window"
      : "Waiting for the first market frame";

  const peakLabel = peak
    ? `${days > 1 ? `${day(peak.t)} ` : ""}${rangeLabel(peak.t, frame.ms)}`
    : "—";

  return (
    <section className="panel min-w-0" data-accent="pink" aria-label="Market activity">
      <div className="panel-head flex-wrap gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-clock" className="h-4 w-4 text-pink" />
          Market activity
        </h2>
        <div className="flex flex-wrap items-center gap-2">
          <Segmented
            label="Period"
            value={period}
            onChange={setPeriod}
            options={EVENT_PERIODS}
          />
          <Segmented
            label="Bucket"
            value={timeframe}
            onChange={setTimeframe}
            options={Object.entries(EVENT_TIMEFRAMES).map(([key, config]) => ({
              key,
              label: config.label
            }))}
          />
        </div>
      </div>

      <div className="px-4 pb-4 pt-3">
        <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span>
            Events{" "}
            <strong className="tabular-nums text-pink">{formatInt(total)}</strong>
          </span>
          <span>
            Peak{" "}
            <strong className="tabular-nums text-gold">{formatInt(peak?.n ?? 0)}</strong>{" "}
            <span className="text-faint">{peak ? peakLabel : ""}</span>
          </span>
          <span>
            Avg/bar <strong className="tabular-nums text-ink-dim">{average.toFixed(1)}</strong>
          </span>
          {status === "stale" ? (
            <span className="ml-auto text-warn" title={error || "Retrying"}>
              Stale · retrying
            </span>
          ) : (
            <span className="ml-auto text-faint" aria-hidden="true">
              {days}D · {frame.minutes}M BARS
            </span>
          )}
        </div>

        <div
          className="relative mt-2 h-[140px] sm:h-[168px]"
          role="img"
          aria-label={`Market activity over ${days} day${days === 1 ? "" : "s"} in ${frame.minutes} minute bars: ${formatInt(total)} events${
            peak ? `, busiest ${peakLabel} with ${formatInt(peak.n)}` : ""
          }`}
        >
          {overlay ? (
            <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center bg-night-950/70 px-3 text-center font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
              {overlay}
            </div>
          ) : null}
          <ResponsiveContainer width="100%" height="100%">
            {/* Right margin keeps the `d/M HH:00` label on the last tick from
                being clipped at the plot edge. */}
            <BarChart data={data} margin={{ top: 8, right: 38, left: 0, bottom: 0 }} barCategoryGap={0}>
              <defs>
                <linearGradient id={`activity-${gradientId}`} x1="0" y1="0" x2="0" y2="1">
                  <stop offset="0%" stopColor="#ff6bcb" stopOpacity={0.95} />
                  <stop offset="100%" stopColor="#ff6bcb" stopOpacity={0.35} />
                </linearGradient>
              </defs>
              <XAxis
                dataKey="t"
                type="number"
                scale="time"
                domain={["dataMin", "dataMax"]}
                tickFormatter={(value) => axisTick(value, days)}
                tick={{
                  fill: "#8576c4",
                  fontSize: 12,
                  fontFamily: "JetBrains Mono, monospace",
                  letterSpacing: "0.02em"
                }}
                tickLine={false}
                axisLine={false}
                minTickGap={36}
                padding={{ left: 0, right: 0 }}
              />
              <YAxis hide domain={[0, yMax]} />
              <Tooltip
                content={<EventTooltip bucketMs={frame.ms} />}
                cursor={{ fill: "rgba(255, 107, 203, 0.08)" }}
                wrapperStyle={{ outline: "none" }}
              />
              <Bar dataKey="n" isAnimationActive={false} maxBarSize={data.length > 240 ? 4 : 18}>
                {data.map((point) => (
                  <Cell
                    key={point.t}
                    fill={
                      point.t === busiest && point.n > 0
                        ? "#ffd23f"
                        : `url(#activity-${gradientId})`
                    }
                  />
                ))}
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      </div>
    </section>
  );
}
