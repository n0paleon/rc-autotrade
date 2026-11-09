import { test } from "bun:test";
import assert from "node:assert/strict";

import { ttfbAt } from "../src/lib/timings.js";

test("anchors at the monotonic TTFB instead of the response mark", () => {
  assert.equal(
    ttfbAt({ sendAt: 1_000, responseAt: 1_120, ttfbMs: 100, fillAt: 1_300 }),
    1_100
  );
});

test("keeps the retry response mark because sendAt is the first dispatch", () => {
  assert.equal(
    ttfbAt({ sendAt: 1_000, responseAt: 1_200, ttfbMs: 80, attempts: 2 }),
    1_200
  );
});

test("falls back to responseAt for rows without a TTFB sample", () => {
  assert.equal(ttfbAt({ sendAt: 1_000, responseAt: 1_120 }), 1_120);
  assert.equal(ttfbAt({ sendAt: 1_000, responseAt: 1_120, ttfbMs: -1 }), 1_120);
  assert.equal(ttfbAt({ responseAt: 1_120 }), 1_120);
});

test("returns null when the purchase never received a response", () => {
  assert.equal(ttfbAt(null), null);
  assert.equal(ttfbAt({}), null);
});

test("can reconstruct the boundary from the TTFB sample alone", () => {
  assert.equal(ttfbAt({ sendAt: 1_000, ttfbMs: 100 }), 1_100);
});
