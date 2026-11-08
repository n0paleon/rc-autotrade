import { lazy, memo, Suspense, useEffect, useMemo, useState } from "react";

import { Icon, ModalShell } from "./ModalShell.jsx";
import { TELEMETRY_PUSH_MS } from "../lib/constants.js";
import {
  formatBytes,
  formatInt,
  formatMs,
  formatPercent,
  formatPreciseMs,
  formatUptime,
  latencyChannelLabel,
  latencyClass,
  relTime,
  sparkBars,
  thresholdClass,
  utilizationClass
} from "../lib/format.js";

const LATENCY_CHANNELS = ["http", "keepalive", "wsHandshake", "wsPing"];

// Recharts is the heaviest dependency; the chart only loads when the monitor
// window opens (or the user expands it), exactly like the price chart.
const OrderBenchmarkChart = lazy(() => import("./OrderBenchmarkChart.jsx"));

function ChartSkeleton({ className = "h-[116px]" }) {
  return (
    <div
      className={`flex items-center justify-center border-2 border-line/60 bg-night-950/50 font-mono text-[11px] uppercase tracking-[0.08em] text-faint ${className}`}
    >
      Loading chart…
    </div>
  );
}

/* ── upstream latency ─────────────────────────────────────────────── */

function LatencyChannel({ label, stats }) {
  const bars = sparkBars(stats?.recent || []);
  const count = stats?.count || 0;
  const errors = stats?.errors || 0;
  const metrics = [
    ["Avg", stats?.avg],
    ["P50", stats?.p50],
    ["P95", stats?.p95, latencyClass(stats?.p95)],
    ["Min", stats?.min],
    ["Max", stats?.max],
    ["Jitter", stats?.jitter]
  ];

  return (
    <article className="lat-card @container" aria-label={`${label} latency`}>
      <div className="flex items-baseline justify-between gap-3">
        <div className="min-w-0">
          <h3 className="label truncate">{label}</h3>
          <span className="mt-1 block font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            {formatInt(count)} {count === 1 ? "sample" : "samples"}
            {errors > 0 ? <b className="text-danger"> · {formatInt(errors)} err</b> : null}
          </span>
        </div>
        <b className={`shrink-0 font-mono text-[20px] font-bold leading-none tabular-nums ${latencyClass(stats?.last) || "text-muted"}`}>
          {formatMs(stats?.last)}
        </b>
      </div>
      <div className="lat-spark mt-2.5" aria-hidden="true">
        {bars.length === 0 ? (
          <span className="lat-spark-empty">No samples yet</span>
        ) : (
          bars.map((bar, index) => (
            <i
              key={index}
              className={latencyClass(bar.value)}
              style={{ height: `${bar.height}%` }}
              title={formatMs(bar.value)}
            />
          ))
        )}
      </div>
      <dl className="mt-2.5 grid grid-cols-3 gap-1.5 @[27rem]:grid-cols-6">
        {metrics.map(([key, value, tone]) => (
          <div key={key} className="lat-metric">
            <dt>{key}</dt>
            <dd className={tone || ""}>{formatMs(value)}</dd>
          </div>
        ))}
      </dl>
      {errors > 0 ? (
        <p className="mt-2 flex items-start gap-1.5 font-mono text-[11px] leading-snug text-danger">
          <Icon name="i-alert" className="mt-px h-3 w-3 shrink-0" />
          <span className="break-words">{stats.lastError || "Upstream call failed"}</span>
        </p>
      ) : null}
    </article>
  );
}

function LatencyPanel({ latency, fetchLatency, refreshLatency, notify }) {
  const channels = latency?.channels || {};
  const fresh = latency?.freshest || null;
  const [probing, setProbing] = useState(false);

  // Safety net: the worker pushes latency on every telemetry tick, but poll
  // quietly too so the panel stays fresh even if the push stream is missed.
  useEffect(() => {
    if (typeof fetchLatency !== "function") return undefined;
    const timer = setInterval(() => {
      fetchLatency().catch(() => {
        /* the push stream or a reconnect will deliver it — stay silent */
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [fetchLatency]);

  const runProbe = async () => {
    setProbing(true);
    try {
      const result = await refreshLatency();
      if (result?.probe?.skipped) {
        notify("info", "PROBE SKIPPED", "Keepalive probe is disabled (or mock mode).");
      } else if (result?.probe?.ok) {
        notify("success", "PROBE DONE", "Fresh upstream sample recorded.");
      } else {
        notify("warn", "PROBE FAILED", result?.probe?.error || "No response from upstream.");
      }
    } catch (error) {
      notify("error", "PROBE FAILED", error.message);
    } finally {
      setProbing(false);
    }
  };

  return (
    <section className="panel @container" data-accent="cyan">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-wifi" className="h-4 w-4 text-info" /> Upstream latency</h2>
        <span
          className="tag hidden shrink-0 gap-1.5 text-info @[19rem]:inline-flex"
          title={`The worker pushes latency every ${TELEMETRY_PUSH_MS / 1000} seconds; this panel polls it as a fallback`}
        >
          <span className="led led-pulse" aria-hidden="true" /> Auto · {TELEMETRY_PUSH_MS / 1000}s
        </span>
      </div>
      <div className="panel-body">
        <div className="flex flex-wrap items-end justify-between gap-3 border-2 border-line bg-night-950/70 p-3">
          <div className="min-w-0">
            <span className="label flex items-center gap-1.5"><Icon name="i-wifi" className="h-3 w-3" /> Worker → upstream</span>
            <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <b className={`font-mono text-[28px] font-bold leading-none tabular-nums ${fresh ? latencyClass(fresh.last) : "text-muted"}`}>
                {fresh ? formatMs(fresh.last) : "—"}
              </b>
              <span className="min-w-0 truncate font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
                {fresh ? `${latencyChannelLabel(fresh.channel)} · ${relTime(fresh.lastAt)}` : "Waiting for the first sample"}
              </span>
            </div>
          </div>
          <button className="btn btn-sm" type="button" onClick={runProbe} disabled={probing} data-busy={probing ? "true" : undefined} aria-busy={probing}>
            {probing ? "Probing…" : "Probe now"}
          </button>
        </div>
        <div className="grid grid-cols-1 gap-2 @[37rem]:grid-cols-2">
          {LATENCY_CHANNELS.map((key) => (
            <LatencyChannel key={key} label={latencyChannelLabel(key)} stats={channels[key] || { count: 0, recent: [] }} />
          ))}
        </div>
        <p className="text-[13px] leading-relaxed text-faint">
          HTTP covers purchases, item info and token refresh. Warm probe is the keepalive GET. WS handshake and ping cover the market feed. Failures still count because they used upstream time.
        </p>
      </div>
    </section>
  );
}

/* ── order benchmark ──────────────────────────────────────────────── */

const BENCHMARK_STAGES = [
  { key: "delivery", label: "Feed → engine", hint: "Upstream frame received until the engine handler starts" },
  { key: "decision", label: "Signal evaluated", hint: "Target match, depth pricing, gates and cooldown" },
  { key: "commit", label: "Order commit", hint: "Counter update, SQLite write and the UI order event" },
  { key: "submit", label: "Submit → fetch", hint: "Purchase payload built and the request dispatched" },
  { key: "ttfb", label: "Purchase → TTFB", hint: "Dispatch until the first response byte; retry excluded" },
  { key: "body", label: "Body parsed", hint: "First byte until the response body is read and parsed" },
  { key: "settle", label: "Result handled", hint: "HTTP result until accepted or rejected" },
  { key: "fillWait", label: "Await fill", hint: "Marketplace matching until the fulfillment frame" },
  { key: "fillHandle", label: "Fill handled", hint: "Fulfillment frame until the loot row is written" },
  { key: "total", label: "Signal → done", hint: "Whole pipeline, from the signal to the final mark" }
];

const BENCHMARK_COLUMNS = [
  ["avg", "Avg", true],
  ["p50", "P50", false],
  ["p95", "P95", true],
  ["last", "Last", false],
  ["jitter", "Jitter", true],
  ["max", "Max", false]
];

/** One order stage is "slow" on a different scale than a network call. */
function benchmarkTone(key, value) {
  if (key === "fillWait" || key === "total") {
    return thresholdClass(value, 2_000, 5_000);
  }
  return thresholdClass(value, 250, 800);
}

/** A success rate is higher-is-better, unlike every latency bracket. */
function successTone(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return "text-muted";
  if (number < 60) return "text-danger";
  if (number < 90) return "text-warn";
  return "text-signal";
}

function BenchmarkPanel({ benchmark, fetchBenchmark, onExpandChart }) {
  const data = benchmark || null;
  const windows = data?.windows || {};
  // The compact panel is a fixed 50-order summary. Range controls live in the
  // expanded chart, where time period, bucket and stage make sense.
  const activeKey = windows["50"] ? "50" : Object.keys(windows)[0];
  const active = windows[activeKey] || { samples: 0, stages: {} };
  const stages = active.stages || {};
  const totals = data?.totals || {};
  const series = data?.series || [];

  // Same safety net as the other telemetry panels: the worker pushes this on
  // every tick, but a missed push should not freeze the window.
  useEffect(() => {
    if (typeof fetchBenchmark !== "function") return undefined;
    const timer = setInterval(() => {
      fetchBenchmark().catch(() => {
        /* a reconnect or the push stream will refresh it */
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [fetchBenchmark]);

  const slowest = useMemo(() => {
    let best = null;
    for (const stage of BENCHMARK_STAGES) {
      if (stage.key === "total") continue;
      const avg = Number(stages[stage.key]?.avg);
      if (!Number.isFinite(avg) || avg <= 0) continue;
      if (!best || avg > best.avg) best = { ...stage, avg };
    }
    return best;
  }, [stages]);

  const total = stages.total || {};
  const samples = active.samples || 0;

  return (
    <section className="panel @container" data-accent="gold">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-pulse" className="h-4 w-4 text-gold" /> Order benchmark</h2>
        <span
          className="tag hidden shrink-0 gap-1.5 text-gold @[19rem]:inline-flex"
          title={`Rebuilt from the loot log and pushed every ${TELEMETRY_PUSH_MS / 1000} seconds`}
        >
          <span className="led led-pulse" aria-hidden="true" /> Auto · {TELEMETRY_PUSH_MS / 1000}s
        </span>
      </div>
      <div className="panel-body">
        <div className="flex flex-wrap items-end justify-between gap-3 border-2 border-line bg-night-950/70 p-3">
          <div className="min-w-0">
            <span className="label flex items-center gap-1.5"><Icon name="i-bolt" className="h-3 w-3" /> End-to-end per order</span>
            <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <b className={`font-mono text-[28px] font-bold leading-none tabular-nums ${benchmarkTone("total", total.avg)}`}>
                {formatPreciseMs(total.avg)}
              </b>
              <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">avg</span>
              <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
                · P95 <b className={`tabular-nums ${benchmarkTone("total", total.p95)}`}>{formatPreciseMs(total.p95)}</b>
                {" · "}jitter <b className="tabular-nums text-ink-dim">{formatPreciseMs(total.jitter)}</b>
              </span>
            </div>
          </div>
        </div>

        <dl className="grid grid-cols-2 gap-1.5 @[30rem]:grid-cols-4">
          {[
            ["Orders", formatInt(totals.orders || 0), "text-ink-dim"],
            ["Filled", formatInt(totals.filled || 0), "text-signal"],
            ["Failed", formatInt(totals.failed || 0), totals.failed > 0 ? "text-danger" : "text-ink-dim"],
            ["Success", formatPercent(totals.successRate, 1), successTone(totals.successRate)]
          ].map(([label, value, tone]) => (
            <div key={label} className="lat-metric">
              <dt>{label}</dt>
              <dd className={tone}>{value}</dd>
            </div>
          ))}
        </dl>

        <Suspense fallback={<ChartSkeleton />}>
          <OrderBenchmarkChart
            mode="preview"
            series={series}
            p95={total.p95}
            onExpand={() => onExpandChart(true)}
          />
        </Suspense>

        <div className="overflow-x-auto border-2 border-line">
          <table className="w-full min-w-[21rem] border-collapse font-mono text-[12px] tabular-nums sm:min-w-[34rem]">
            <caption className="sr-only">Order pipeline stages for the last {samples} timed orders</caption>
            <thead>
              <tr className="border-b-2 border-line bg-night-900/80 text-[11px] uppercase tracking-[0.08em] text-faint">
                <th scope="col" className="px-2.5 py-1.5 text-left font-medium">Stage</th>
                <th scope="col" className="px-2 py-1.5 text-right font-medium">n</th>
                {BENCHMARK_COLUMNS.map(([key, label, mobile]) => (
                  <th key={key} scope="col" className={`px-2 py-1.5 text-right font-medium${mobile ? "" : " hidden sm:table-cell"}`}>{label}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {BENCHMARK_STAGES.map((stage) => {
                const stat = stages[stage.key] || {};
                const empty = !stat.count;
                return (
                  <tr
                    key={stage.key}
                    className={`border-b border-line/60 last:border-b-0 ${
                      stage.key === "total" ? "bg-night-900/50 font-semibold text-ink" : "text-ink-dim"
                    }`}
                    title={stage.hint}
                  >
                    <th scope="row" className="whitespace-nowrap px-2.5 py-1.5 text-left font-normal">
                      {stage.label}
                      {slowest?.key === stage.key ? <span className="ml-1.5 text-[10px] uppercase text-warn">slowest</span> : null}
                    </th>
                    <td className="px-2 py-1.5 text-right text-faint">{empty ? "—" : formatInt(stat.count)}</td>
                    {BENCHMARK_COLUMNS.map(([key, , mobile]) => (
                      <td key={key} className={`whitespace-nowrap px-2 py-1.5 text-right ${key === "p95" ? benchmarkTone(stage.key, stat[key]) : ""}${mobile ? "" : " hidden sm:table-cell"}`}>
                        {formatPreciseMs(stat[key])}
                      </td>
                    ))}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        <p className="text-[13px] leading-relaxed text-faint">
          {samples > 0
            ? `Showing the last ${formatInt(samples)} timed orders.`
            : "No timed orders yet."}{" "}
          Metrics come from the loot log&apos;s timing marks and survive a restart. Jitter is the
          population standard deviation, so it matches the upstream latency cards.
          {slowest ? (
            <span className="text-muted"> Slowest stage: <b className="text-warn">{slowest.label}</b>{" "}
              <span className="font-mono tabular-nums">{formatPreciseMs(slowest.avg)}</span> avg.
            </span>
          ) : null}
        </p>
      </div>
    </section>
  );
}

/* ── worker health ────────────────────────────────────────────────── */

function SystemMeter({ label, value, detail, tone }) {
  const number = Number(value);
  const width = Number.isFinite(number) ? Math.max(0, Math.min(100, number)) : 0;
  const fill =
    tone === "text-danger" ? "bg-danger" : tone === "text-warn" ? "bg-warn" : "bg-signal";
  return (
    <div className="border-2 border-line bg-night-950/60 p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="label">{label}</span>
        <b className={`font-mono text-[14px] font-semibold tabular-nums ${tone || "text-ink-dim"}`}>
          {formatPercent(value, 1)}
        </b>
      </div>
      <div className="meter meter-thin mt-2">
        <i className={fill} style={{ "--meter-value": `${width}%` }} />
      </div>
      <span className="mt-1.5 block font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
        {detail}
      </span>
    </div>
  );
}

function StatTile({ label, value, tone, meta }) {
  return (
    <span className="flex min-w-0 flex-col gap-0.5 border border-line/70 bg-night-900/60 px-1.5 py-1 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
      <span className="truncate leading-tight">{label}</span>
      <b className={`whitespace-nowrap text-[12px] font-semibold leading-tight tracking-normal tabular-nums ${tone || "text-ink-dim"}`}>
        {value}
      </b>
      {meta ? <span className="whitespace-nowrap text-[11px] leading-tight tracking-[0.04em]">{meta}</span> : null}
    </span>
  );
}

function HistoryStrip({ label, bars, format, tone }) {
  return (
    <div className="@container border-2 border-line bg-night-950/60 p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="label min-w-0 whitespace-nowrap">{label}</span>
        <span className="hidden shrink-0 whitespace-nowrap font-mono text-[12px] uppercase tracking-[0.08em] text-faint @[20rem]:inline">
          {bars.length} samples
        </span>
      </div>
      <div className="mt-2 flex h-10 items-end gap-[2px]" aria-hidden="true">
        {bars.length === 0 ? (
          <span className="font-mono text-[12px] uppercase tracking-wide text-faint">Collecting…</span>
        ) : (
          bars.map((bar, index) => (
            <i
              key={index}
              className={`max-w-[10px] min-w-[2px] flex-1 bg-current ${tone ? tone(bar.value) : "text-signal"}`}
              style={{ height: `${bar.height}%` }}
              title={format(bar.value)}
            />
          ))
        )}
      </div>
    </div>
  );
}

function SystemPanel({ system, fetchSystem }) {
  const health = system?.health || { level: "ok", reasons: [] };
  const proc = system?.process || {};
  const cpu = system?.cpu || {};
  const memory = system?.memory || {};
  const host = system?.system || {};
  const loop = system?.eventLoop || {};
  const lag = loop.lag || {};
  const history = system?.history || [];
  const db = system?.db || null;
  const dbFiles = db?.files || {};
  const dbSqlite = db?.sqlite || {};
  const dbQueries = db?.queries || {};
  const dbWindow = db?.window || {};
  const dbTransactions = db?.transactions || {};
  const dbWarn = Number(db?.thresholds?.p95WarnMs) || 25;
  const dbCritical = Number(db?.thresholds?.p95CriticalMs) || 100;

  // Safety net: the worker pushes `system` alongside gateway/latency/benchmark
  // on every telemetry tick. Quietly poll as well so the panel survives a
  // missed push after a reconnect.
  useEffect(() => {
    if (typeof fetchSystem !== "function") return undefined;
    const timer = setInterval(() => {
      fetchSystem().catch(() => {
        /* a reconnect or the push stream will refresh it */
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [fetchSystem]);

  if (!system) {
    return (
      <section className="panel" data-accent="lime">
        <div className="panel-head">
          <h2 className="panel-title"><Icon name="i-bolt" className="h-4 w-4 text-signal" /> Worker health</h2>
        </div>
        <div className="panel-body">
          <p className="text-[13px] leading-relaxed text-faint">
            Waiting for the first metrics sample. The worker samples CPU, memory and event-loop
            health every few seconds; make sure it is running the latest build.
          </p>
        </div>
      </section>
    );
  }

  const healthLabel = health.level === "critical" ? "CRITICAL" : health.level === "warn" ? "DEGRADED" : "HEALTHY";
  const healthChip =
    health.level === "critical" ? "chip-offline" : health.level === "warn" ? "chip-warn" : "chip-online";
  const heapTone = utilizationClass(memory.heapPercent, 80, 92);
  const hostTone = utilizationClass(host.usedPercent, 88, 96);
  const cpuTone = thresholdClass(cpu.processPercent, 80, 100);
  const loadPerCore = Number.isFinite(Number(cpu.load1)) && cpu.cores ? cpu.load1 / cpu.cores : null;

  return (
    <section className="panel @container" data-accent="lime">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-bolt" className="h-4 w-4 text-signal" /> Worker health</h2>
        <span className={`chip ${healthChip}`} title={health.reasons?.join(" · ") || "All health checks passed"}>
          <span className="led led-pulse" />{healthLabel}
        </span>
      </div>
      <div className="panel-body">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="label flex items-center gap-1.5">
            <Icon name="i-clock" className="h-3 w-3" /> Uptime {formatUptime(proc.uptimeMs)}
          </span>
          <span className="font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
            PID {proc.pid} · {proc.runtime || proc.node}
          </span>
        </div>

        {health.level !== "ok" ? (
          <p className="border-2 border-line bg-night-950/60 px-3 py-2 font-mono text-[12px] uppercase tracking-[0.08em] text-warn">
            {health.reasons?.length ? health.reasons.join(" · ") : "Health check degraded"}
          </p>
        ) : null}

        <div className="grid grid-cols-1 gap-2 @2xl:grid-cols-3">
          <SystemMeter
            label="Worker CPU"
            value={cpu.processPercent}
            detail={`${formatInt(cpu.cores)} cores · host ${formatPercent(cpu.hostPercent, 1)}`}
            tone={cpuTone}
          />
          <SystemMeter
            label="Heap"
            value={memory.heapPercent}
            detail={`${formatBytes(memory.heapUsed)} / ${formatBytes(memory.heapLimit)}`}
            tone={heapTone}
          />
          <SystemMeter
            label="System RAM"
            value={host.usedPercent}
            detail={`${formatBytes(host.used)} / ${formatBytes(host.total)}`}
            tone={hostTone}
          />
        </div>

        <div className="grid grid-cols-3 gap-1.5 @xl:grid-cols-6">
          {[
            ["RSS", formatBytes(memory.rss), "text-ink-dim"],
            ["External", formatBytes(memory.external), "text-ink-dim"],
            ["Free RAM", formatBytes(host.available), thresholdClass(host.usedPercent, 88, 96)],
            ["Load 1m", Number.isFinite(Number(cpu.load1)) ? Number(cpu.load1).toFixed(2) : "—", thresholdClass(loadPerCore ? loadPerCore * 100 : null, 70, 95)],
            ["Loop p95", formatMs(lag.p95), thresholdClass(lag.p95, 100, 250)],
            ["Handles", proc.activeHandles == null ? "—" : formatInt(proc.activeHandles), "text-ink-dim"]
          ].map(([label, value, tone]) => (
            <StatTile key={label} label={label} value={value} tone={tone} />
          ))}
        </div>

        <div className="border-2 border-line bg-night-950/60 p-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="label flex items-center gap-1.5">
              <Icon name="i-download" className="h-3 w-3" /> SQLite · {dbSqlite.driver || "—"} ·{" "}
              {dbSqlite.journalMode || "—"}
            </span>
            <span
              className={`font-mono text-[12px] uppercase tracking-[0.08em] ${
                db?.instrumented ? "text-signal" : "text-faint"
              }`}
              title="Statements are timed through wrapped prepare/transaction calls"
            >
              {db?.instrumented ? "instrumented" : "not instrumented"}
            </span>
          </div>
          <div className="mt-2 grid grid-cols-2 gap-1.5 @xs:grid-cols-4">
            {[
              ["DB", formatBytes(dbFiles.db), "text-ink-dim"],
              ["WAL", formatBytes(dbFiles.wal), thresholdClass(dbFiles.wal, db?.thresholds?.walWarnBytes, db?.thresholds?.walCriticalBytes)],
              ["Q/S", Number(dbQueries.rate).toFixed(1), "text-violet"],
              ["P95", formatPreciseMs(dbWindow.p95Ms), thresholdClass(dbWindow.p95Ms, dbWarn, dbCritical)],
              ["AVG", formatPreciseMs(dbWindow.avgMs), "text-ink-dim"],
              ["MAX", formatPreciseMs(dbWindow.maxMs), thresholdClass(dbWindow.maxMs, dbWarn * 4, dbCritical)],
              ["TX", formatInt(dbTransactions.count || 0), "text-ink-dim"],
              [
                "ERR",
                formatInt((dbWindow.errors || 0) + (dbWindow.busy || 0)),
                dbWindow.errors + dbWindow.busy > 0 ? "text-danger" : "text-ink-dim"
              ]
            ].map(([label, value, tone]) => (
              <StatTile key={label} label={label} value={value} tone={tone} />
            ))}
          </div>
          {db?.recentSlow?.length ? (
            <div className="mt-2 space-y-1">
              {db.recentSlow.slice(0, 2).map((item, index) => (
                <div
                  key={`${item.at}-${index}`}
                  className="flex items-baseline gap-2 font-mono text-[11px] uppercase tracking-[0.06em]"
                >
                  <b className="shrink-0 text-warn">{formatPreciseMs(item.ms)}</b>
                  <code className="truncate text-faint" title={item.sql}>
                    {item.sql}
                  </code>
                </div>
              ))}
            </div>
          ) : (
            <span className="mt-1.5 block font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
              {formatInt(dbWindow.count || 0)} statements · no slow queries in the last 60s
            </span>
          )}
        </div>

        <div className="grid grid-cols-1 gap-2 @xs:grid-cols-2">
          <HistoryStrip
            label="CPU"
            bars={sparkBars(history.map((point) => point.cpu ?? 0))}
            format={(value) => formatPercent(value, 1)}
            tone={(value) => thresholdClass(value, 80, 100)}
          />
          <HistoryStrip
            label="RSS"
            bars={sparkBars(history.map((point) => point.rss ?? 0))}
            format={(value) => formatBytes(value * 1024 * 1024)}
            tone={() => "text-info"}
          />
          <HistoryStrip
            label="SQL"
            bars={sparkBars((db?.history || []).map((point) => point.qps ?? 0))}
            format={(value) => `${Number(value).toFixed(1)}/s`}
            tone={() => "text-violet"}
          />
        </div>

        <p className="text-[13px] leading-relaxed text-faint">
          Sampled every {formatInt(Math.round((system.intervalMs || 2000) / 1000)) || 1}s, pushed every{" "}
          {TELEMETRY_PUSH_MS / 1000}s. CPU is the worker process as a percentage of one core; system
          RAM uses MemAvailable on Linux. Warnings trigger at 80% heap, 88% RAM, 100ms event-loop p95, a DB p95 above{" "}
          {formatInt(dbWarn)}ms, or a WAL above{" "}
          {formatBytes(db?.thresholds?.walWarnBytes || 64 * 1024 * 1024)}. Query timings include only
          the SQL text, never bound values.
        </p>
      </div>
    </section>
  );
}

/* ── window shell ─────────────────────────────────────────────────── */

// Each panel only changes when its own 3 s telemetry slice arrives. Memoising
// them keeps a market tick (which redraws the deck several times a second)
// from repainting the whole window.
const MemoBenchmarkPanel = memo(BenchmarkPanel);
const MemoLatencyPanel = memo(LatencyPanel);
const MemoSystemPanel = memo(SystemPanel);

/**
 * One wide window for the three telemetry surfaces that used to be cramped
 * into sidebar tabs beside the feed. The panels keep their container queries,
 * so they reflow on their own when the window is narrow or full screen.
 */
export function MonitorWindow({ open, snapshot, worker, notify, onClose }) {
  const [chartExpanded, setChartExpanded] = useState(false);
  const benchmark = snapshot.benchmark || null;
  const stamp = benchmark?.measuredAt || snapshot.now || null;

  // The window stays mounted while closed, so a full-size chart must not be
  // the first thing the next open shows.
  useEffect(() => {
    if (!open) setChartExpanded(false);
  }, [open]);

  if (!open) return null;

  return (
    <ModalShell
      title={chartExpanded ? "Order latency · expanded" : "Monitor window"}
      titleColor="lime"
      size="monitor"
      onClose={onClose}
      footer={
        chartExpanded ? null : (
          <div className="flex flex-wrap items-center justify-between gap-2 font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            <span>
              <span className="led led-pulse text-signal" /> Telemetry live · push every {TELEMETRY_PUSH_MS / 1000}s
            </span>
            <span>
              {benchmark
                ? `Order benchmark updated ${stamp ? relTime(stamp) : "—"} · ${formatInt(benchmark.available || 0)} timed orders`
                : "Waiting for benchmark telemetry"}
            </span>
          </div>
        )
      }
    >
      {chartExpanded ? (
        <Suspense fallback={<div className="p-4"><ChartSkeleton className="h-[60vh]" /></div>}>
          <OrderBenchmarkChart
            mode="detail"
            request={worker.request}
            benchmark={benchmark}
            onCollapse={setChartExpanded}
          />
        </Suspense>
      ) : (
        <div className="monitor-window space-y-3 p-3 sm:p-4">
          <div className="flex flex-wrap items-center justify-between gap-2 border-2 border-line bg-night-950/70 px-3 py-2">
            <span className="flex items-center gap-2 font-mono text-[11px] font-bold uppercase tracking-[0.12em] text-gold">
              <Icon name="i-pulse" className="h-3.5 w-3.5" /> Orders · latency · system
            </span>
            <span className="flex flex-wrap items-center gap-2 font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
              <span className="flex items-center gap-1"><span className="led bg-info" /> upstream</span>
              <span className="flex items-center gap-1"><span className="led bg-gold" /> orders</span>
              <span className="flex items-center gap-1"><span className="led bg-signal" /> worker</span>
            </span>
          </div>

          <div className="grid grid-cols-1 items-start gap-3 xl:grid-cols-2">
            <div className="flex min-w-0 flex-col gap-3">
              <MemoBenchmarkPanel
                benchmark={benchmark}
                fetchBenchmark={worker.fetchBenchmark}
                onExpandChart={setChartExpanded}
              />
              <MemoLatencyPanel
                latency={snapshot.latency}
                fetchLatency={worker.fetchLatency}
                refreshLatency={worker.refreshLatency}
                notify={notify}
              />
            </div>
            <MemoSystemPanel system={snapshot.system} fetchSystem={worker.fetchSystem} />
          </div>
        </div>
      )}
    </ModalShell>
  );
}
