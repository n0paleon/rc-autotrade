/**
 * Worker health monitor.
 *
 * Samples process and host metrics on a fixed cadence so the control deck can
 * answer "is the worker healthy?" without shelling out to `top`, `pm2 monit`
 * or a separate metrics stack:
 *
 *   process     pid, uptime, node version, active handles/requests
 *   cpu         worker CPU (percent of one core), host CPU (all cores), load
 *   memory      rss, heap used/limit, external, array buffers
 *   system      host RAM and swap (MemAvailable on Linux, not just MemFree)
 *   eventLoop   event-loop utilization and delay percentiles in milliseconds
 *   gc          every V8 garbage-collection pause (kind + duration) so a
 *               latency spike can be matched to a major GC
 *   health      ok / warn / critical derived from the metrics above
 *
 * Every dependency can be injected so the sampler is deterministic in tests.
 * History is bounded and in-memory only; nothing is persisted to SQLite.
 */

import { readFileSync } from "node:fs";
import os from "node:os";
import v8 from "node:v8";
import {
  PerformanceObserver,
  constants as perfConstants,
  monitorEventLoopDelay,
  performance
} from "node:perf_hooks";

const DEFAULT_INTERVAL_MS = 2_000;
const MIN_INTERVAL_MS = 500;
const MAX_INTERVAL_MS = 60_000;
const DEFAULT_HISTORY = 120;
const MAX_HISTORY = 600;
const BYTES_PER_MB = 1024 * 1024;
const DEFAULT_GC_HISTORY = 200;

/** Health thresholds. Percentages are 0..100, delays are milliseconds. */
export const SYSTEM_THRESHOLDS = Object.freeze({
  heapWarn: 80,
  heapCritical: 92,
  systemWarn: 88,
  systemCritical: 96,
  loopWarn: 100,
  loopCritical: 250,
  gcWarn: 120,
  gcCritical: 300,
  cpuWarn: 95
});

/** V8 GC kinds as reported by `entry.detail.kind`. */
export const GC_KIND_LABELS = Object.freeze({
  [perfConstants.NODE_PERFORMANCE_GC_MINOR]: "minor",
  [perfConstants.NODE_PERFORMANCE_GC_MAJOR]: "major",
  [perfConstants.NODE_PERFORMANCE_GC_INCREMENTAL]: "incremental",
  [perfConstants.NODE_PERFORMANCE_GC_WEAKCB]: "weakcb"
});

export function gcKindLabel(kind) {
  return GC_KIND_LABELS[kind] || "gc";
}

function round(value, digits = 1) {
  const number = Number(value);
  if (!Number.isFinite(number)) return null;
  const factor = 10 ** digits;
  return Math.round(number * factor) / factor;
}

function percentOf(part, total) {
  if (!Number.isFinite(part) || !Number.isFinite(total) || total <= 0) return null;
  return round((part / total) * 100, 1);
}

function toMb(bytes) {
  const number = Number(bytes);
  return Number.isFinite(number) && number >= 0 ? round(number / BYTES_PER_MB, 1) : null;
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function safeCall(fn, fallback) {
  try {
    return fn();
  } catch {
    return fallback;
  }
}

/**
 * Parses /proc/meminfo so "used" reflects MemAvailable instead of MemFree.
 * os.freemem() only counts entirely free pages, which makes a healthy Linux
 * box look like it is at 90% memory usage because of page cache.
 */
export function readSystemMemory() {
  if (process.platform === "linux") {
    const info = safeCall(() => {
      const values = {};
      const text = readFileSync("/proc/meminfo", "utf8");
      for (const line of text.split("\n")) {
        const match = /^([A-Za-z0-9_()]+):\s+(\d+)\s*kB\s*$/.exec(line.trim());
        if (!match) continue;
        values[match[1]] = Number(match[2]) * 1024;
      }
      return values;
    }, null);

    if (info && Number.isFinite(info.MemTotal) && info.MemTotal > 0) {
      const cached =
        (info.Cached || 0) + (info.SReclaimable || 0) + (info.Buffers || 0);
      const available = Number.isFinite(info.MemAvailable)
        ? info.MemAvailable
        : (info.MemFree || 0) + cached;
      return {
        source: "proc",
        total: info.MemTotal,
        available,
        free: info.MemFree || 0,
        buffers: info.Buffers || 0,
        cached: info.Cached || 0,
        swapTotal: info.SwapTotal || 0,
        swapFree: info.SwapFree || 0
      };
    }
  }

  return {
    source: "os",
    total: os.totalmem(),
    available: os.freemem(),
    free: os.freemem(),
    buffers: 0,
    cached: 0,
    swapTotal: 0,
    swapFree: 0
  };
}

function sumCpuTimes(cpus) {
  let idle = 0;
  let total = 0;
  if (!Array.isArray(cpus)) return { idle, total };
  for (const cpu of cpus) {
    const times = cpu?.times || {};
    const idleTime = Number(times.idle) || 0;
    idle += idleTime;
    total +=
      idleTime +
      (Number(times.user) || 0) +
      (Number(times.nice) || 0) +
      (Number(times.sys) || 0) +
      (Number(times.irq) || 0);
  }
  return { idle, total };
}

function readActiveHandles() {
  if (typeof process.getActiveResourcesInfo === "function") {
    const info = safeCall(() => process.getActiveResourcesInfo(), null);
    return Array.isArray(info) ? info.length : null;
  }
  const handles = safeCall(() => process._getActiveHandles?.(), null);
  return Array.isArray(handles) ? handles.length : null;
}

function readActiveRequests() {
  const requests = safeCall(() => process._getActiveRequests?.(), null);
  return Array.isArray(requests) ? requests.length : null;
}

function createHistogram() {
  return safeCall(() => monitorEventLoopDelay({ resolution: 20 }), null);
}

/**
 * Collects V8 GC events from the perf_hooks PerformanceObserver.
 *
 * Node emits a `gc` performance entry for every collection: a short scavenge
 * (minor) runs often, while a mark-sweep (major) can stall the event loop for
 * tens to hundreds of milliseconds. Those stalls are exactly what makes the
 * control link or WS ping appear to spike, so every event keeps its wall-clock
 * timestamp, kind and pause duration.
 *
 * Wire-format is deliberately small: a bounded list of events plus aggregates
 * for the last 60s (window) and the last 15s (spike).
 */
export class GcTracker {
  constructor({
    capacity = DEFAULT_GC_HISTORY,
    now = () => Date.now(),
    observer = null,
    timeOrigin = Date.now() - performance.now(),
    pauseWarnMs = SYSTEM_THRESHOLDS.gcWarn,
    pauseCriticalMs = SYSTEM_THRESHOLDS.gcCritical
  } = {}) {
    const limit = Math.floor(Number(capacity));
    this.capacity = Number.isFinite(limit) ? clamp(limit, 2, 1_000) : DEFAULT_GC_HISTORY;
    this.now = now;
    this.observer = observer;
    this.timeOrigin = Number.isFinite(Number(timeOrigin)) ? Number(timeOrigin) : 0;
    this.pauseWarnMs = Number.isFinite(Number(pauseWarnMs)) ? Number(pauseWarnMs) : SYSTEM_THRESHOLDS.gcWarn;
    this.pauseCriticalMs = Number.isFinite(Number(pauseCriticalMs))
      ? Number(pauseCriticalMs)
      : SYSTEM_THRESHOLDS.gcCritical;

    this.events = [];
    this.started = false;
    this.observing = false;
    this.totalCount = 0;
    this.totalPausedMs = 0;
    this.maxMs = 0;
    this.last = null;
  }

  start() {
    if (this.started) return this;
    this.started = true;
    if (!this.observer && typeof PerformanceObserver === "function") {
      this.observer = safeCall(
        () =>
          new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) this.record(entry);
          }),
        null
      );
    }
    if (this.observer) {
      this.observing = safeCall(() => {
        this.observer.observe({ entryTypes: ["gc"] });
        return true;
      }, false);
    }
    return this;
  }

  stop() {
    if (this.observer) safeCall(() => this.observer.disconnect(), null);
    this.observing = false;
    this.started = false;
  }

  /** Record one PerformanceEntry (or a test double with the same fields). */
  record(entry) {
    if (!entry) return null;
    const duration = Number(entry.duration);
    if (!Number.isFinite(duration) || duration < 0) return null;

    const kindCode = Number(entry.detail?.kind ?? entry.kind);
    const startTime = Number(entry.startTime);
    const at = Number.isFinite(startTime)
      ? Math.round(this.timeOrigin + startTime)
      : this.now();
    const event = {
      at,
      kind: gcKindLabel(kindCode),
      kindCode: Number.isFinite(kindCode) ? kindCode : null,
      duration: round(duration, 2),
      flags: Number(entry.detail?.flags) || 0
    };

    this.events.push(event);
    if (this.events.length > this.capacity) {
      this.events.splice(0, this.events.length - this.capacity);
    }
    this.totalCount += 1;
    this.totalPausedMs = round(this.totalPausedMs + event.duration, 2);
    this.maxMs = Math.max(this.maxMs, event.duration);
    this.last = event;
    return event;
  }

  /** Aggregate every recorded event in [sinceMs, untilMs]. */
  window(sinceMs, untilMs = this.now()) {
    const since = Number.isFinite(Number(sinceMs)) ? Number(sinceMs) : 0;
    const until = Number.isFinite(Number(untilMs)) ? Number(untilMs) : this.now();
    const events = this.events.filter((event) => event.at >= since && event.at <= until);

    let pausedMs = 0;
    let maxMs = 0;
    let minor = 0;
    let major = 0;
    let incremental = 0;
    let weakcb = 0;
    let other = 0;
    const durations = [];
    for (const event of events) {
      pausedMs += event.duration;
      maxMs = Math.max(maxMs, event.duration);
      durations.push(event.duration);
      if (event.kind === "minor") minor += 1;
      else if (event.kind === "major") major += 1;
      else if (event.kind === "incremental") incremental += 1;
      else if (event.kind === "weakcb") weakcb += 1;
      else other += 1;
    }

    let p95Ms = 0;
    if (durations.length > 0) {
      const sorted = durations.sort((a, b) => a - b);
      p95Ms = sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))];
    }

    return {
      since,
      until,
      count: events.length,
      minor,
      major,
      incremental,
      weakcb,
      other,
      pausedMs: round(pausedMs, 2),
      maxMs: round(maxMs, 2),
      p95Ms: round(p95Ms, 2)
    };
  }

  snapshot({ windowMs = 60_000, spikeMs = 15_000 } = {}) {
    const now = this.now();
    return {
      observed: Boolean(this.started && this.observing),
      supported: typeof PerformanceObserver === "function",
      windowMs,
      spikeMs,
      thresholds: { warnMs: this.pauseWarnMs, criticalMs: this.pauseCriticalMs },
      window: this.window(now - windowMs, now),
      spike: this.window(now - spikeMs, now),
      totals: {
        count: this.totalCount,
        pausedMs: this.totalPausedMs,
        maxMs: this.maxMs,
        last: this.last ? { ...this.last } : null
      },
      // Newest first: the panel shows what just happened, not ancient history.
      recent: this.events.slice(-10).reverse()
    };
  }

  reset() {
    this.events.length = 0;
    this.totalCount = 0;
    this.totalPausedMs = 0;
    this.maxMs = 0;
    this.last = null;
  }
}

/**
 * Streaming health sampler. Call `start()` once and `snapshot()` whenever a
 * client needs the current view; the returned object is JSON-safe.
 */
export class SystemMonitor {
  constructor({
    intervalMs = DEFAULT_INTERVAL_MS,
    historyLimit = DEFAULT_HISTORY,
    now = () => Date.now(),
    cpuUsage = () => process.cpuUsage(),
    memoryUsage = () => process.memoryUsage(),
    heapStatistics = () => v8.getHeapStatistics(),
    cpus = () => os.cpus(),
    loadavg = () => os.loadavg(),
    systemMemory = () => readSystemMemory(),
    processUptime = () => process.uptime() * 1000,
    systemUptime = () => os.uptime() * 1000,
    eventLoopUtilization = () => performance.eventLoopUtilization(),
    activeHandles = () => readActiveHandles(),
    activeRequests = () => readActiveRequests(),
    histogram = undefined,
    gc = undefined
  } = {}) {
    const interval = Math.floor(Number(intervalMs));
    this.intervalMs = Number.isFinite(interval)
      ? clamp(interval, MIN_INTERVAL_MS, MAX_INTERVAL_MS)
      : DEFAULT_INTERVAL_MS;

    const limit = Math.floor(Number(historyLimit));
    this.historyLimit = Number.isFinite(limit)
      ? clamp(limit, 2, MAX_HISTORY)
      : DEFAULT_HISTORY;

    this.now = now;
    this.deps = {
      cpuUsage,
      memoryUsage,
      heapStatistics,
      cpus,
      loadavg,
      systemMemory,
      processUptime,
      systemUptime,
      eventLoopUtilization,
      activeHandles,
      activeRequests
    };

    this.histogram = histogram === undefined ? createHistogram() : histogram;
    this.histogramEnabled = false;

    // `gc: null` disables GC tracking (used by deterministic unit tests that
    // do not care about it); otherwise the monitor owns the tracker.
    this.gc = gc === undefined ? new GcTracker({ now }) : gc;

    this.previous = null;
    this.previousElu = null;
    this.previousGcAt = null;
    this.latest = null;
    this.history = [];
    this.timer = null;
  }

  start() {
    if (this.timer) return this;
    if (this.histogram && !this.histogramEnabled) {
      safeCall(() => this.histogram.enable(), null);
      this.histogramEnabled = true;
    }
    this.gc?.start();
    this.sample(); // baseline for CPU deltas
    this.timer = setInterval(() => {
      try {
        this.sample();
      } catch {
        /* monitoring must never take the worker down */
      }
    }, this.intervalMs);
    this.timer.unref?.();
    return this;
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.gc?.stop();
    if (this.histogram && this.histogramEnabled) {
      safeCall(() => this.histogram.disable(), null);
      this.histogramEnabled = false;
    }
  }

  /** Take one sample, update history, and return the public snapshot. */
  sample() {
    const at = this.now();
    const usage = safeCall(() => this.deps.cpuUsage(), null) || {};
    const cpus = safeCall(() => this.deps.cpus(), []) || [];
    const cpuTimes = sumCpuTimes(cpus);
    const wallMs = this.previous ? at - this.previous.at : 0;

    let processPercent = null;
    let hostPercent = null;
    if (this.previous && wallMs > 0) {
      const userDelta = (Number(usage.user) || 0) - (Number(this.previous.usage.user) || 0);
      const systemDelta = (Number(usage.system) || 0) - (Number(this.previous.usage.system) || 0);
      const usedMs = (userDelta + systemDelta) / 1000;
      processPercent = round((usedMs / wallMs) * 100, 1);
      if (processPercent !== null) processPercent = Math.max(0, processPercent);

      const idleDelta = cpuTimes.idle - this.previous.cpuTimes.idle;
      const totalDelta = cpuTimes.total - this.previous.cpuTimes.total;
      if (totalDelta > 0) {
        hostPercent = round(((totalDelta - idleDelta) / totalDelta) * 100, 1);
        if (hostPercent !== null) hostPercent = clamp(hostPercent, 0, 100);
      }
    }
    this.previous = { at, usage, cpuTimes };

    const memory = safeCall(() => this.deps.memoryUsage(), null) || {};
    const heapStats = safeCall(() => this.deps.heapStatistics(), null) || {};
    const systemRaw = safeCall(() => this.deps.systemMemory(), null) || {};
    const loadRaw = safeCall(() => this.deps.loadavg(), []) || [];

    const total = Number(systemRaw.total);
    const available = Number(systemRaw.available ?? systemRaw.free);
    const used = Number.isFinite(total) && Number.isFinite(available) ? Math.max(0, total - available) : null;
    const swapTotal = Number(systemRaw.swapTotal) || 0;
    const swapFree = Number(systemRaw.swapFree) || 0;

    const heapLimit = Number(heapStats.heap_size_limit) || 0;
    const heapUsed = Number(memory.heapUsed);
    const rss = Number(memory.rss);
    const rssMb = toMb(rss);
    const heapUsedMb = toMb(heapUsed);

    const loop = this.#readLoopDelay();
    const utilization = this.#readEventLoopUtilization();

    const cpu = {
      processPercent,
      hostPercent,
      cores: cpus.length || 1,
      model: String(cpus[0]?.model || "unknown").replace(/\s+/g, " ").trim(),
      load1: round(loadRaw[0], 2),
      load5: round(loadRaw[1], 2),
      load15: round(loadRaw[2], 2)
    };

    const memoryView = {
      rss,
      rssMb,
      heapTotal: Number(memory.heapTotal) || 0,
      heapUsed: heapUsed || 0,
      heapUsedMb,
      heapLimit,
      external: Number(memory.external) || 0,
      arrayBuffers: Number(memory.arrayBuffers) || 0,
      heapPercent: percentOf(heapUsed, heapLimit),
      rssPercent: percentOf(rss, total)
    };

    const systemView = {
      source: systemRaw.source || "unknown",
      total: Number.isFinite(total) ? total : null,
      available: Number.isFinite(available) ? available : null,
      used,
      free: Number(systemRaw.free) || null,
      usedPercent: percentOf(used, total),
      swapTotal,
      swapUsed: swapTotal > 0 ? Math.max(0, swapTotal - swapFree) : 0,
      swapPercent: swapTotal > 0 ? percentOf(Math.max(0, swapTotal - swapFree), swapTotal) : null
    };

    const eventLoop = {
      utilization: utilization?.utilization ?? null,
      lag: loop
    };

    // GC pause inside this sample interval. Events are timestamped with the
    // actual GC start time, so late PerformanceObserver delivery still lands
    // in the right bucket.
    const gcSince = this.previousGcAt ?? at - this.intervalMs;
    const gcWindow = this.gc ? this.gc.window(gcSince, at) : null;
    this.previousGcAt = at;

    const gc = this.gc ? this.gc.snapshot() : null;
    const health = classifyHealth({ cpu, memory: memoryView, system: systemView, eventLoop, gc });

    this.history.push({
      at,
      cpu: processPercent,
      rss: rssMb,
      heap: heapUsedMb,
      loop: loop?.p95 ?? null,
      gc: gcWindow ? gcWindow.pausedMs : null,
      gcCount: gcWindow ? gcWindow.count : null
    });
    if (this.history.length > this.historyLimit) {
      this.history.splice(0, this.history.length - this.historyLimit);
    }

    this.latest = {
      at,
      cpu,
      memory: memoryView,
      system: systemView,
      eventLoop,
      gc,
      health
    };

    return this.snapshot();
  }

  snapshot() {
    if (!this.latest) this.sample();
    const at = this.latest.at;
    const handles = safeCall(() => this.deps.activeHandles(), null);
    const requests = safeCall(() => this.deps.activeRequests(), null);

    return {
      sampledAt: at,
      intervalMs: this.intervalMs,
      process: {
        pid: process.pid,
        node: process.version,
        platform: process.platform,
        arch: process.arch,
        uptimeMs: Math.round(Number(safeCall(() => this.deps.processUptime(), 0)) || 0),
        systemUptimeMs: Math.round(Number(safeCall(() => this.deps.systemUptime(), 0)) || 0),
        activeHandles: handles,
        activeRequests: requests
      },
      cpu: { ...this.latest.cpu },
      memory: { ...this.latest.memory },
      system: { ...this.latest.system },
      eventLoop: {
        utilization: this.latest.eventLoop.utilization,
        lag: this.latest.eventLoop.lag ? { ...this.latest.eventLoop.lag } : null
      },
      gc: this.latest.gc
        ? {
            ...this.latest.gc,
            window: { ...this.latest.gc.window },
            spike: { ...this.latest.gc.spike },
            totals: {
              ...this.latest.gc.totals,
              last: this.latest.gc.totals.last ? { ...this.latest.gc.totals.last } : null
            },
            recent: this.latest.gc.recent.map((event) => ({ ...event }))
          }
        : null,
      health: {
        level: this.latest.health.level,
        reasons: [...this.latest.health.reasons]
      },
      history: this.history.map((entry) => ({ ...entry })),
      historyLimit: this.historyLimit
    };
  }

  #readLoopDelay() {
    const histogram = this.histogram;
    if (!histogram) return null;
    const count = Number(histogram.count) || 0;
    if (count === 0) return null;
    const nsToMs = (value) => round((Number(value) || 0) / 1e6, 1);
    const out = {
      count,
      mean: nsToMs(histogram.mean),
      p50: nsToMs(histogram.percentile(50)),
      p95: nsToMs(histogram.percentile(95)),
      p99: nsToMs(histogram.percentile(99)),
      max: nsToMs(histogram.max)
    };
    safeCall(() => histogram.reset(), null);
    return out;
  }

  #readEventLoopUtilization() {
    const current = safeCall(() => this.deps.eventLoopUtilization(), null);
    if (!current) return null;
    const previous = this.previousElu;
    this.previousElu = current;
    if (!previous) return null;

    const active = (Number(current.active) || 0) - (Number(previous.active) || 0);
    const idle = (Number(current.idle) || 0) - (Number(previous.idle) || 0);
    const total = active + idle;
    return {
      utilization: total > 0 ? round((Math.max(0, active) / total) * 100, 1) : null
    };
  }
}

/**
 * Maps raw metrics to a coarse health level. Only the metrics that actually
 * destabilise a Node process raise the alarm: heap pressure, host RAM
 * exhaustion, a long GC pause, and event-loop stall. CPU is advisory because
 * bursts are normal for a purchase engine.
 */
export function classifyHealth({ cpu, memory, system, eventLoop, gc } = {}) {
  const reasons = [];
  let level = "ok";
  const escalate = (next) => {
    if (next === "critical") level = "critical";
    else if (next === "warn" && level !== "critical") level = "warn";
  };

  const heap = Number(memory?.heapPercent);
  if (Number.isFinite(heap)) {
    if (heap >= SYSTEM_THRESHOLDS.heapCritical) {
      escalate("critical");
      reasons.push(`heap ${heap.toFixed(1)}% of limit`);
    } else if (heap >= SYSTEM_THRESHOLDS.heapWarn) {
      escalate("warn");
      reasons.push(`heap ${heap.toFixed(1)}% of limit`);
    }
  }

  const sys = Number(system?.usedPercent);
  if (Number.isFinite(sys)) {
    if (sys >= SYSTEM_THRESHOLDS.systemCritical) {
      escalate("critical");
      reasons.push(`system memory ${sys.toFixed(1)}% used`);
    } else if (sys >= SYSTEM_THRESHOLDS.systemWarn) {
      escalate("warn");
      reasons.push(`system memory ${sys.toFixed(1)}% used`);
    }
  }

  const lag = Number(eventLoop?.lag?.p95);
  if (Number.isFinite(lag)) {
    if (lag >= SYSTEM_THRESHOLDS.loopCritical) {
      escalate("critical");
      reasons.push(`event loop p95 ${lag.toFixed(0)}ms`);
    } else if (lag >= SYSTEM_THRESHOLDS.loopWarn) {
      escalate("warn");
      reasons.push(`event loop p95 ${lag.toFixed(0)}ms`);
    }
  }

  // Only the last ~15s of GC activity raises health, so a single big GC does
  // not keep the worker "critical" for a whole minute after it recovered.
  const gcWarn = Number(gc?.thresholds?.warnMs) || SYSTEM_THRESHOLDS.gcWarn;
  const gcCritical = Number(gc?.thresholds?.criticalMs) || SYSTEM_THRESHOLDS.gcCritical;
  const gcMax = Number(gc?.spike?.maxMs ?? gc?.maxMs);
  if (Number.isFinite(gcMax) && gcMax > 0) {
    if (gcMax >= gcCritical) {
      escalate("critical");
      reasons.push(`GC pause ${gcMax.toFixed(0)}ms`);
    } else if (gcMax >= gcWarn) {
      escalate("warn");
      reasons.push(`GC pause ${gcMax.toFixed(0)}ms`);
    }
  }

  const cpuPercent = Number(cpu?.processPercent);
  if (Number.isFinite(cpuPercent) && cpuPercent >= SYSTEM_THRESHOLDS.cpuWarn) {
    escalate("warn");
    reasons.push(`worker cpu ${cpuPercent.toFixed(0)}%`);
  }

  return { level, reasons };
}
