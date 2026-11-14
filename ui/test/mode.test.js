import { test } from "bun:test";
import assert from "node:assert/strict";

import { DEFAULT_SETTINGS, MODES, modeOf } from "../src/lib/constants.js";

test("offers exactly the three trading modes", () => {
  assert.deepEqual(
    MODES.map((mode) => mode.id),
    ["off", "shadow", "live"]
  );
  assert.equal(new Set(MODES.map((mode) => mode.id)).size, 3);
  for (const mode of MODES) {
    assert.ok(mode.label && mode.hint, `${mode.id} needs a label and a hint`);
  }
});

test("defaults to live, matching the backend", () => {
  assert.equal(DEFAULT_SETTINGS.mode, "live");
  assert.equal(DEFAULT_SETTINGS.dryRun, undefined);
  assert.equal(DEFAULT_SETTINGS.paused, undefined);
  // Fulfillment headroom ships off so an upgrade cannot silently raise the
  // price ceiling of every order.
  assert.equal(DEFAULT_SETTINGS.totalPriceMarkupPct, 0);
});

test("reads the new mode field", () => {
  assert.equal(modeOf({ mode: "off" }), "off");
  assert.equal(modeOf({ mode: "shadow" }), "shadow");
  assert.equal(modeOf({ mode: "live" }), "live");
});

test("an explicit mode beats stale legacy flags", () => {
  assert.equal(modeOf({ mode: "shadow", dryRun: false, paused: false }), "shadow");
  assert.equal(modeOf({ mode: "live", dryRun: true, paused: true }), "live");
});

test("falls back to the legacy pair when mode is absent", () => {
  assert.equal(modeOf({ paused: true, dryRun: true }), "off", "the dead cell is off");
  assert.equal(modeOf({ paused: false, dryRun: true }), "shadow");
  assert.equal(modeOf({ paused: false, dryRun: false }), "live");
  // A missing payload must not be read as armed for live.
  assert.equal(modeOf({}), "live", "no fields at all keeps the backend default");
  assert.equal(modeOf(), "live");
  assert.equal(modeOf({ mode: "bogus" }), "live");
});
