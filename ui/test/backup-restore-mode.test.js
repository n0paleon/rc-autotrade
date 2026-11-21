// A backup is data, not permission to trade. The worker ignores any mode in the
// file and restores in shadow, so the optimistic snapshot the deck shows while
// the import is in flight must say shadow too — even when an older file still
// carries "live".
//
// useWorker.js imports browser globals at module scope (via ./lib/sound.js), so
// the storage objects are stubbed before the module is imported.
import { test, expect } from "bun:test";

function makeStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
    _map: map
  };
}

globalThis.localStorage = makeStorage();
globalThis.sessionStorage = makeStorage();
globalThis.window = { location: { origin: "http://127.0.0.1:8787", protocol: "http:" } };

const { optimisticBackupSnapshot } = await import("../src/useWorker.js");

const previous = {
  settings: {
    delayPerOrder: 200,
    maxOrdersPerMinute: 60,
    mode: "live",
    autoRefresh: true,
    totalPriceMarkupPct: 0
  },
  session: null
};

test("an imported backup is previewed as shadow even when the file says live", () => {
  const next = optimisticBackupSnapshot(
    previous,
    {
      data: {
        settings: {
          delayPerOrder: 250,
          maxOrdersPerMinute: 30,
          mode: "live",
          dryRun: false,
          paused: false,
          autoRefresh: false,
          totalPriceMarkupPct: 2.5
        },
        stats: { since: 1 },
        targets: [],
        orders: [],
        reportProducts: [],
        reportSince: 1
      }
    },
    false
  );

  expect(next.settings.mode).toBe("shadow");
  // The non-trading settings still come from the file.
  expect(next.settings.delayPerOrder).toBe(250);
  expect(next.settings.maxOrdersPerMinute).toBe(30);
  expect(next.settings.autoRefresh).toBe(false);
  expect(next.settings.totalPriceMarkupPct).toBe(2.5);
});

test("a backup without a settings mode still previews as shadow, not the default", () => {
  const next = optimisticBackupSnapshot(
    previous,
    { data: { settings: { delayPerOrder: 300 }, stats: { since: 1 }, targets: [] } },
    false
  );

  expect(next.settings.mode).toBe("shadow");
  expect(next.settings.delayPerOrder).toBe(300);
});
