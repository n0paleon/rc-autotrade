import test from "node:test";
import assert from "node:assert/strict";

import {
  GcTracker,
  SystemMonitor,
  classifyHealth,
  gcKindLabel,
  readSystemMemory
} from "../src/worker/system.js";

const MB = 1024 * 1024;
const GC_MAJOR = 4;
const GC_MINOR = 1;

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
    // GC tracking is exercised explicitly below; keep the shared fixture
    // deterministic by disabling it by default.
    gc: null,
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
  assert.equal(view.gc, null, "GC tracking disabled by the fixture");
  assert.deepEqual(
    Object.keys(view.history[0]).sort(),
    ["at", "cpu", "gc", "gcCount", "heap", "loop", "rss"]
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

test("GcTracker records kinds, totals and windowed aggregates", () => {
  const tracker = new GcTracker({
    now: () => 10_000,
    timeOrigin: 0,
    capacity: 10,
    observer: null
  });

  tracker.record({ duration: 1.234, detail: { kind: GC_MINOR, flags: 0 }, startTime: 9_000 });
  tracker.record({ duration: 42.5, detail: { kind: GC_MAJOR, flags: 4 }, startTime: 9_500 });
  assert.equal(tracker.record(null), null);
  assert.equal(tracker.record({ duration: Number.NaN }), null);
  assert.equal(tracker.record({ duration: -1 }), null);

  const snapshot = tracker.snapshot({ windowMs: 60_000, spikeMs: 15_000 });
  assert.equal(snapshot.window.count, 2);
  assert.equal(snapshot.window.minor, 1);
  assert.equal(snapshot.window.major, 1);
  assert.equal(snapshot.window.pausedMs, 43.73);
  assert.equal(snapshot.window.maxMs, 42.5);
  assert.equal(snapshot.window.p95Ms, 42.5);
  assert.equal(snapshot.totals.count, 2);
  assert.equal(snapshot.totals.pausedMs, 43.73);
  assert.equal(snapshot.totals.maxMs, 42.5);
  assert.equal(snapshot.totals.last.kind, "major");
  assert.equal(snapshot.totals.last.at, 9_500);
  assert.equal(snapshot.recent.length, 2);
  assert.equal(snapshot.recent[0].kind, "major", "recent is newest first");
  assert.deepEqual(snapshot.thresholds, { warnMs: 120, criticalMs: 300 });

  const narrow = tracker.window(9_400, 10_000);
  assert.equal(narrow.count, 1);
  assert.equal(narrow.maxMs, 42.5);
  assert.equal(tracker.window(20_000, 30_000).count, 0);
});

test("GcTracker keeps a bounded ring buffer without losing totals", () => {
  const tracker = new GcTracker({ now: () => 5_000, timeOrigin: 0, capacity: 2 });
  for (let index = 0; index < 4; index += 1) {
    tracker.record({ duration: index + 1, detail: { kind: GC_MINOR }, startTime: 1_000 + index });
  }

  assert.deepEqual(tracker.events.map((event) => event.duration), [3, 4]);
  assert.equal(tracker.snapshot().totals.count, 4);
  assert.equal(tracker.snapshot().totals.pausedMs, 10);
});

test("GcTracker observes perf entries and disconnects cleanly", () => {
  const calls = [];
  const observer = {
    observe: (options) => calls.push(["observe", options]),
    disconnect: () => calls.push(["disconnect"])
  };
  const tracker = new GcTracker({ now: () => 1_000, observer });

  tracker.start();
  tracker.start(); // idempotent: observe is called once
  assert.equal(tracker.snapshot().observed, true);
  tracker.stop();

  assert.deepEqual(calls, [["observe", { entryTypes: ["gc"] }], ["disconnect"]]);
  assert.equal(tracker.snapshot().observed, false);
});

test("gcKindLabel maps V8 kinds and falls back safely", () => {
  assert.equal(gcKindLabel(GC_MINOR), "minor");
  assert.equal(gcKindLabel(GC_MAJOR), "major");
  assert.equal(gcKindLabel(8), "incremental");
  assert.equal(gcKindLabel(16), "weakcb");
  assert.equal(gcKindLabel(999), "gc");
  assert.equal(gcKindLabel(undefined), "gc");
});

test("SystemMonitor attributes GC pauses to the sample that saw them", () => {
  const observer = { observe: () => {}, disconnect: () => {} };
  let gcNow = 1_000_000;
  const gc = new GcTracker({ now: () => gcNow, timeOrigin: 0, observer });
  gc.start();

  const { monitor, state } = harness({ gc });
  monitor.sample();
  assert.equal(monitor.snapshot().history.at(-1).gc, 0);

  advance(state, { ms: 2_000, user: 1_000, system: 1_000 });
  gcNow = state.at;
  gc.record({ duration: 30, detail: { kind: GC_MAJOR }, startTime: 1_001_000 });
  const view = monitor.sample();

  assert.equal(view.gc.observed, true);
  assert.equal(view.gc.window.count, 1);
  assert.equal(view.gc.spike.maxMs, 30);
  assert.equal(view.history.at(-1).gc, 30);
  assert.equal(view.history.at(-1).gcCount, 1);
  assert.equal(view.health.level, "ok", "30ms is below the warning threshold");
});

test("classifyHealth escalates heap, host memory, loop lag, GC and CPU", () => {
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

  const gcWarn = classifyHealth({
    gc: { spike: { maxMs: 150 }, thresholds: { warnMs: 120, criticalMs: 300 } }
  });
  assert.equal(gcWarn.level, "warn");
  assert.match(gcWarn.reasons[0], /GC pause 150ms/);
  assert.equal(classifyHealth({ gc: { spike: { maxMs: 350 } } }).level, "critical");
  assert.equal(
    classifyHealth({ gc: { window: { maxMs: 400 }, spike: { maxMs: 0 } } }).level,
    "ok",
    "an old GC pause must not pin health forever"
  );

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
