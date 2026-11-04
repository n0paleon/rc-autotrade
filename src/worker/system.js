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
 *   db          SQLite file sizes, query latency, slow statements, WAL state
 *   health      ok / warn / critical derived from the metrics above
 *
 * Every dependency can be injected so the sampler is deterministic in tests.
 * History is bounded and in-memory only; nothing is persisted to SQLite.
 */

import { readFileSync } from "node:fs";
import os from "node:os";
import v8 from "node:v8";
import { monitorEventLoopDelay, performance } from "node:perf_hooks";
import { DB_THRESHOLDS } from "./db-metrics.js";

const DEFAULT_INTERVAL_MS = 2_000;
const MIN_INTERVAL_MS = 500;
const MAX_INTERVAL_MS = 60_000;
const DEFAULT_HISTORY = 120;
const MAX_HISTORY = 600;
const BYTES_PER_MB = 1024 * 1024;

/** Health thresholds. Percentages are 0..100, delays are milliseconds. */
export const SYSTEM_THRESHOLDS = Object.freeze({
  heapWarn: 80,
  heapCritical: 92,
  systemWarn: 88,
  systemCritical: 96,
  loopWarn: 100,
  loopCritical: 250,
  cpuWarn: 95
});

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

/** Deep-ish copy of a DbMetrics view so callers cannot mutate the sample. */
function copyDbView(view) {
  return {
    ...view,
    files: { ...view.files },
    storage: { ...view.storage },
    sqlite: { ...view.sqlite },
    queries: { ...view.queries },
    transactions: { ...view.transactions },
    window: { ...view.window },
    thresholds: { ...view.thresholds },
    recentSlow: view.recentSlow.map((item) => ({ ...item })),
    history: view.history.map((point) => ({ ...point }))
  };
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
    db = null
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

    // The DB probe is instrumented by the store; the monitor only samples it.
    this.db = db;

    this.previous = null;
    this.previousElu = null;
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

    const dbView = this.db ? this.db.sample() : null;
    const health = classifyHealth({
      cpu,
      memory: memoryView,
      system: systemView,
      eventLoop,
      db: dbView
    });

    this.history.push({
      at,
      cpu: processPercent,
      rss: rssMb,
      heap: heapUsedMb,
      loop: loop?.p95 ?? null
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
      db: dbView,
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
        runtime:
          typeof Bun !== "undefined" && Bun.version
            ? `bun ${Bun.version}`
            : `node ${process.version}`,
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
      db: this.latest.db ? copyDbView(this.latest.db) : null,
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
 * exhaustion, event-loop stall, and slow SQLite queries or a
 * bloated WAL. CPU is advisory because bursts are normal for a purchase engine.
 */
export function classifyHealth({ cpu, memory, system, eventLoop, db } = {}) {
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

  const cpuPercent = Number(cpu?.processPercent);
  if (Number.isFinite(cpuPercent) && cpuPercent >= SYSTEM_THRESHOLDS.cpuWarn) {
    escalate("warn");
    reasons.push(`worker cpu ${cpuPercent.toFixed(0)}%`);
  }

  // SQLite: only the rolling window raises health, so one old slow checkpoint
  // does not pin the worker in a warning state forever.
  const dbWarnMs = Number(db?.thresholds?.p95WarnMs) || DB_THRESHOLDS.p95WarnMs;
  const dbCriticalMs = Number(db?.thresholds?.p95CriticalMs) || DB_THRESHOLDS.p95CriticalMs;
  const dbP95 = Number(db?.window?.p95Ms);
  if (Number.isFinite(dbP95) && dbP95 > 0) {
    if (dbP95 >= dbCriticalMs) {
      escalate("critical");
      reasons.push(`db p95 ${dbP95.toFixed(0)}ms`);
    } else if (dbP95 >= dbWarnMs) {
      escalate("warn");
      reasons.push(`db p95 ${dbP95.toFixed(0)}ms`);
    }
  }

  const walBytes = Number(db?.files?.wal);
  const walWarn = Number(db?.thresholds?.walWarnBytes) || DB_THRESHOLDS.walWarnBytes;
  const walCritical = Number(db?.thresholds?.walCriticalBytes) || DB_THRESHOLDS.walCriticalBytes;
  if (Number.isFinite(walBytes) && walBytes > 0) {
    if (walBytes >= walCritical) {
      escalate("critical");
      reasons.push(`WAL ${(walBytes / (1024 * 1024)).toFixed(0)}MB`);
    } else if (walBytes >= walWarn) {
      escalate("warn");
      reasons.push(`WAL ${(walBytes / (1024 * 1024)).toFixed(0)}MB`);
    }
  }

  const dbErrors = (Number(db?.window?.errors) || 0) + (Number(db?.window?.busy) || 0);
  if (dbErrors > 0) {
    escalate("warn");
    reasons.push(`db errors ${dbErrors}`);
  }

  return { level, reasons };
}
