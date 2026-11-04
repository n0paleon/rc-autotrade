import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  SystemMonitor,
  classifyHealth,
  readSystemMemory
} from "../src/worker/system.js";

const MB = 1024 * 1024;

/**
 * Builds a monitor with fully deterministic inputs plus a mutable `state`
 * object the tests advance between samples.
 */
function harness(overrides = {}) {
  const state = {
    at: 1_000_000,
    usage: { user: 0, system: 0 },
    cpus: [
      {
        model: "Test Core i7 @ 3.00GHz",
        times: { user: 1_000, nice: 0, sys: 500, idle: 5_000, irq: 0 }
      }
    ],
    elu: { active: 1_000, idle: 9_000 }
  };

  const monitor = new SystemMonitor({
    now: () => state.at,
    cpuUsage: () => ({ ...state.usage }),
    cpus: () => state.cpus.map((cpu) => ({ ...cpu, times: { ...cpu.times } })),
    memoryUsage: () => ({
      rss: 120 * MB,
      heapTotal: 90 * MB,
      heapUsed: 45 * MB,
      external: 2 * MB,
      arrayBuffers: 1 * MB
    }),
    heapStatistics: () => ({ heap_size_limit: 200 * MB }),
    loadavg: () => [0.75, 0.5, 0.25],
    systemMemory: () => ({
      source: "test",
      total: 1_000 * MB,
      available: 400 * MB,
      free: 100 * MB,
      swapTotal: 100 * MB,
      swapFree: 90 * MB
    }),
    processUptime: () => 65_000,
    systemUptime: () => 7_200_000,
    eventLoopUtilization: () => ({ ...state.elu }),
    activeHandles: () => 4,
    activeRequests: () => 1,
    histogram: null,
    historyLimit: 3,
    ...overrides
  });

  return { monitor, state };
}

function advance(
  state,
  { ms = 1_000, user = 0, system = 0, hostBusy = 0, hostIdle = 0, eluActive = 0, eluIdle = 0 } = {}
) {
  state.at += ms;
  state.usage.user += user;
  state.usage.system += system;
  state.cpus[0].times.user += hostBusy;
  state.cpus[0].times.idle += hostIdle;
  state.elu.active += eluActive;
  state.elu.idle += eluIdle;
}

test("first sample exposes process, memory and host shape without CPU deltas", () => {
  const { monitor } = harness();
  const view = monitor.sample();

  assert.equal(view.cpu.processPercent, null, "no baseline yet");
  assert.equal(view.cpu.hostPercent, null, "no baseline yet");
  assert.equal(view.eventLoop.utilization, null, "no baseline yet");
  assert.equal(view.cpu.cores, 1);
  assert.equal(view.cpu.model, "Test Core i7 @ 3.00GHz");
  assert.equal(view.cpu.load1, 0.75);

  assert.equal(view.memory.rss, 120 * MB);
  assert.equal(view.memory.heapUsed, 45 * MB);
  assert.equal(view.memory.heapLimit, 200 * MB);
  assert.equal(view.memory.heapPercent, 22.5);
  assert.equal(view.memory.rssPercent, 12);

  assert.equal(view.system.total, 1_000 * MB);
  assert.equal(view.system.used, 600 * MB);
  assert.equal(view.system.usedPercent, 60);
  assert.equal(view.system.swapUsed, 10 * MB);
  assert.equal(view.system.swapPercent, 10);

  assert.equal(view.process.pid, process.pid);
  assert.equal(view.process.activeHandles, 4);
  assert.equal(view.process.uptimeMs, 65_000);
  assert.equal(view.eventLoop.lag, null);
  assert.equal(view.health.level, "ok");
  assert.equal(view.history.length, 1);
  assert.deepEqual(
    Object.keys(view.history[0]).sort(),
    ["at", "cpu", "heap", "loop", "rss"]
  );
});

test("CPU and event-loop utilization use wall-clock deltas", () => {
  const { monitor, state } = harness();
  monitor.sample();

  // 0.5s of CPU consumed over 1s of wall time = 50% of one core.
  advance(state, {
    ms: 1_000,
    user: 200_000,
    system: 300_000,
    hostBusy: 500,
    hostIdle: 500,
    eluActive: 10,
    eluIdle: 40
  });
  const view = monitor.sample();

  assert.equal(view.cpu.processPercent, 50);
  assert.equal(view.cpu.hostPercent, 50);
  assert.equal(view.eventLoop.utilization, 20);
});

test("history is bounded by historyLimit and keeps the newest points", () => {
  const { monitor, state } = harness({ historyLimit: 3 });
  for (let index = 0; index < 6; index += 1) {
    advance(state, { ms: 500, user: 10_000, system: 10_000 });
    monitor.sample();
  }

  const view = monitor.snapshot();
  assert.equal(view.history.length, 3);
  assert.equal(view.historyLimit, 3);
  assert.equal(view.history.at(-1).at, state.at);
  assert.ok(view.history.at(-1).cpu > 0);
});

test("snapshot is a stable JSON-safe copy", () => {
  const { monitor } = harness();
  const first = monitor.snapshot();
  const second = monitor.snapshot();

  assert.notEqual(first, second);
  assert.notEqual(first.history, second.history);
  assert.deepEqual(first, second, "two snapshots without new samples must match");
  assert.doesNotThrow(() => JSON.stringify(first));
});

test("SystemMonitor embeds the DB probe view", () => {
  let calls = 0;
  const dbView = {
    instrumented: true,
    files: { db: 1, wal: 2, shm: 0, total: 3 },
    storage: {},
    sqlite: {},
    queries: {},
    transactions: {},
    window: { p95Ms: 0, errors: 0, busy: 0 },
    thresholds: {},
    recentSlow: [],
    history: []
  };
  const { monitor } = harness({
    db: {
      sample: () => {
        calls += 1;
        return dbView;
      }
    }
  });

  const view = monitor.sample();
  assert.equal(calls, 1);
  assert.equal(view.db.files.total, 3);
  assert.equal(view.db.window.p95Ms, 0);
  assert.doesNotThrow(() => JSON.stringify(view));
});

test("classifyHealth escalates heap, host memory, loop lag and CPU", () => {
  assert.equal(classifyHealth({}).level, "ok");
  assert.deepEqual(classifyHealth({}).reasons, []);

  const heapWarn = classifyHealth({ memory: { heapPercent: 85 } });
  assert.equal(heapWarn.level, "warn");
  assert.match(heapWarn.reasons[0], /heap 85\.0%/);

  assert.equal(classifyHealth({ memory: { heapPercent: 95 } }).level, "critical");

  assert.equal(classifyHealth({ system: { usedPercent: 90 } }).level, "warn");
  assert.equal(classifyHealth({ system: { usedPercent: 97 } }).level, "critical");

  assert.equal(classifyHealth({ eventLoop: { lag: { p95: 150 } } }).level, "warn");
  assert.equal(classifyHealth({ eventLoop: { lag: { p95: 300 } } }).level, "critical");

  const dbWarn = classifyHealth({
    db: { window: { p95Ms: 30 }, thresholds: { p95WarnMs: 25, p95CriticalMs: 100 } }
  });
  assert.equal(dbWarn.level, "warn");
  assert.match(dbWarn.reasons[0], /db p95 30ms/);
  assert.equal(
    classifyHealth({ db: { files: { wal: 300 * MB }, thresholds: { walCriticalBytes: 256 * MB } } }).level,
    "critical"
  );
  assert.equal(classifyHealth({ db: { window: { errors: 1 } } }).level, "warn");

  const cpuOnly = classifyHealth({ cpu: { processPercent: 130 } });
  assert.equal(cpuOnly.level, "warn");
  assert.match(cpuOnly.reasons[0], /worker cpu 130%/);

  const critical = classifyHealth({
    memory: { heapPercent: 99 },
    system: { usedPercent: 99 },
    eventLoop: { lag: { p95: 999 } }
  });
  assert.equal(critical.level, "critical");
  assert.equal(critical.reasons.length, 3);
});

test("start is idempotent and stop releases the sampler", () => {
  const { monitor } = harness({ intervalMs: 600_000 });
  assert.equal(monitor.start(), monitor);
  assert.ok(monitor.timer);
  monitor.start();
  monitor.stop();
  assert.equal(monitor.timer, null);
  monitor.stop();
});

test("readSystemMemory reports a coherent total and available figure", () => {
  const memory = readSystemMemory();
  assert.ok(memory.total > 0, "total memory must be positive");
  assert.ok(memory.available >= 0);
  assert.ok(memory.available <= memory.total + 1, "available cannot exceed total");
  assert.ok(["proc", "os"].includes(memory.source));
});
