import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  ACTIVITY_RANGES,
  categoryLabel,
  categoryOf,
  categoryOptions,
  DEFAULT_ACTIVITY_RANGE,
  filterActivityRows,
  nextBatch,
  rangeForPreset,
  rangeSpansDays,
  rowLabel,
  rowMarketplaceUrl,
  sharePercent,
  summarizeActivity,
  typeBreakdown
} from "../src/lib/activity.js";
import { ITEM_TYPE_ORDER, itemTypeLabel } from "../src/lib/format.js";

const DAY_MS = 86_400_000;

const rows = [
  { itemId: "aaa-busy", label: "Epic Hashboard", itemType: "miner", events: 40, events24h: 40, hours: 4, tracked: false },
  { itemId: "bbb-quiet", label: "", itemType: "rack", events: 10, events24h: 6, hours: 2, tracked: true },
  { itemId: "ccc-parts", label: "Copper Coil", itemType: "mutation_component", events: 5, events24h: 0, hours: 1, tracked: false }
];

test("range presets are rolling windows, not calendar days", () => {
  // 00:20 local: a calendar-day window would show 20 minutes of data for "1D".
  const now = new Date(2026, 10, 13, 0, 20, 0, 0).getTime();

  const hour = rangeForPreset("1h", now);
  assert.equal(hour.until, now);
  assert.equal(hour.until - hour.since, 3_600_000, "1H is exactly one hour back");

  const three = rangeForPreset("3h", now);
  assert.equal(three.until - three.since, 3 * 3_600_000);

  const twelve = rangeForPreset("12h", now);
  assert.equal(twelve.until - twelve.since, 12 * 3_600_000);

  const day = rangeForPreset("1d", now);
  assert.equal(day.until - day.since, 86_400_000, "1D is a full day, not midnight-to-now");
  // Every window must be a whole number of 5-minute storage slices.
  for (const key of ["1h", "3h", "12h", "1d"]) {
    const { since, until } = rangeForPreset(key, now);
    assert.equal((until - since) % 300_000, 0, `${key} must divide into 5m slices`);
  }
});

test("an unknown or missing range falls back to the shortest window", () => {
  const now = new Date(2026, 10, 13, 9, 0, 0, 0).getTime();
  for (const key of ["nope", "", null, undefined]) {
    const range = rangeForPreset(key, now);
    assert.equal(range.until - range.since, 3_600_000);
  }
});

test("rangeSpansDays distinguishes a day from a multi-day window", () => {
  const start = new Date(2026, 10, 13, 0, 0, 0, 0).getTime();
  assert.equal(rangeSpansDays(start, start + DAY_MS - 1), false);
  assert.equal(rangeSpansDays(start, start + DAY_MS), true);
  assert.equal(rangeSpansDays(NaN, start), false);
});
test("category options keep every known type and its whole-range count", () => {
  const options = categoryOptions([
    { itemType: "rack", events: 11, items: 2 },
    { itemType: "miner", events: 10, items: 4 },
    { itemType: "weird_new_type", events: 1, items: 1 }
  ]);
  // Static vocabulary first, then feed types this build does not know yet.
  assert.deepEqual(
    options.map((option) => option.key),
    [...ITEM_TYPE_ORDER, "weird_new_type"]
  );
  assert.equal(options.find((option) => option.key === "rack").count, 2);
  // A category with no activity keeps a zero option, so selecting another
  // category never removes it from the menu.
  assert.equal(options.find((option) => option.key === "battery").count, 0);
  assert.deepEqual(
    categoryOptions([]).map((option) => option.key),
    [...ITEM_TYPE_ORDER]
  );
  assert.deepEqual(
    categoryOptions(undefined).map((option) => option.key),
    [...ITEM_TYPE_ORDER]
  );
});

test("category labels reuse the fleet vocabulary", () => {
  assert.equal(categoryLabel("mutation_component"), "parts");
  assert.equal(categoryLabel("miner"), "miners");
  assert.equal(categoryLabel("weird_new_type"), "weird new type");
  assert.equal(categoryLabel(""), "Uncategorised");
  assert.equal(categoryOf({ itemType: "RACK" }), "rack");
});

test("every row resolves to a type badge key, even with no type at all", () => {
  // The Type column renders `.type-badge[data-type=...]`; an unknown or missing
  // type must still produce a stable key instead of an empty attribute, which
  // would silently fall back to the muted default colour on every such row.
  assert.equal(categoryOf({ itemType: "miner" }), "miner");
  assert.equal(categoryOf({ itemType: "  BATTERY  " }), "battery");
  assert.equal(categoryOf({ itemType: "" }), "unknown");
  assert.equal(categoryOf({}), "unknown");
  assert.equal(categoryOf(null), "unknown");
  // The badge shows the type as the deck already names it.
  assert.equal(itemTypeLabel(categoryOf({ itemType: "mutation_component" })), "parts");
  assert.equal(itemTypeLabel(categoryOf({ itemType: "rack" })), "racks");
  assert.equal(itemTypeLabel(categoryOf({ itemType: "miner" })), "miners");
  assert.equal(itemTypeLabel(categoryOf({ itemType: "battery" })), "batteries");
  assert.equal(itemTypeLabel(categoryOf({ itemType: "utility_item" })), "other");
});

test("filtering matches names, ids and categories, mirroring the server", () => {
  assert.deepEqual(
    filterActivityRows(rows, { itemType: "miner" }).map((row) => row.itemId),
    ["aaa-busy"]
  );
  assert.deepEqual(
    filterActivityRows(rows, { query: "  HASHBOARD " }).map((row) => row.itemId),
    ["aaa-busy"]
  );
  assert.deepEqual(
    filterActivityRows(rows, { query: "bbb-" }).map((row) => row.itemId),
    ["bbb-quiet"],
    "an unresolved row is still findable by id"
  );
  assert.deepEqual(
    filterActivityRows(rows, { itemType: "rack", query: "hashboard" }),
    [],
    "filters intersect"
  );
  assert.equal(filterActivityRows(rows).length, 3);
  assert.deepEqual(filterActivityRows(null, { query: "x" }), []);
});

test("an unresolved row keeps a readable fallback and says it is pending", () => {
  const named = rowLabel(rows[0]);
  assert.deepEqual(named, { text: "Epic Hashboard", pending: false });
  // Short ids are shown whole; a marketplace id keeps its head and tail.
  const pending = rowLabel(rows[1]);
  assert.equal(pending.pending, true);
  assert.equal(pending.text, "ITEM BBB-QUIET");
  const long = rowLabel({ itemId: "6319f7baa8ce530569ed16b9" });
  assert.equal(long.text, "ITEM 6319F7…16B9");
  const empty = rowLabel(null);
  assert.equal(empty.pending, true);
});

test("marketplace links carry the resolved type and the raw id", () => {
  assert.equal(
    rowMarketplaceUrl(rows[0]),
    "https://rollercoin.com/marketplace/buy/miner/aaa-busy"
  );
  assert.equal(
    rowMarketplaceUrl({ itemId: "x y", itemType: "" }),
    "https://rollercoin.com/marketplace/buy/mutation_component/x%20y"
  );
});

test("share is a proportion of every frame in the range, not of the leader", () => {
  // The exact numbers from the report that showed 180 of 2 176 as "100%".
  const total = 2_176;
  assert.equal(sharePercent(180, total), 8, "180 of 2 176 is 8%, not 100%");
  assert.equal(sharePercent(1_063, total), 49);
  assert.equal(sharePercent(2176, total), 100);

  // A table whose rows are the whole range sums to about 100% down the column.
  const table = [
    { events: 1_063 },
    { events: 547 },
    { events: 383 },
    { events: 180 },
    { events: 3 }
  ];
  const summed = table.reduce((sum, row) => sum + sharePercent(row.events, total), 0);
  assert.ok(summed >= 99 && summed <= 101, `shares must sum to ~100, got ${summed}`);
});

test("the share column never presents two answers for one row", () => {
  // The column used to pair a bar scaled to the busiest product with a number
  // scaled to the range total, so a row could show a full track labelled "8%".
  // It is a single number now; this pins the value the bar used to contradict.
  const total = 2_176;
  const leader = 1_063;
  assert.equal(sharePercent(leader, total), 49, "the busiest product is 49% of the range");
  assert.ok(sharePercent(leader, total) < 100, "being the leader is not being the whole");
});

test("a row with activity never rounds its share down to zero", () => {
  const total = 5_000;
  assert.equal(sharePercent(1, total), 1, "1/5000 must not print 0%");
  assert.equal(sharePercent(3, total), 1);
  assert.equal(sharePercent(0, total), 0, "no activity is genuinely 0%");
  assert.equal(sharePercent(10, 0), 0, "an empty range cannot divide");
});

test("the summary separates fleet items from off-fleet discoveries", () => {
  assert.deepEqual(summarizeActivity(rows), {
    rows: 3,
    total: 55,
    tracked: 1,
    untracked: 2
  });
  assert.deepEqual(summarizeActivity(null), { rows: 0, total: 0, tracked: 0, untracked: 0 });
});

test("the panel offers the four short rolling ranges", () => {
  assert.deepEqual(
    ACTIVITY_RANGES.map((preset) => preset.key),
    ["1h", "3h", "12h", "1d"]
  );
  assert.deepEqual(
    ACTIVITY_RANGES.map((preset) => preset.label),
    ["1H", "3H", "12H", "1D"]
  );
  assert.deepEqual(
    ACTIVITY_RANGES.map((preset) => preset.ms),
    [3_600_000, 3 * 3_600_000, 12 * 3_600_000, 86_400_000]
  );
  // The default has to be one of the offered windows.
  assert.ok(ACTIVITY_RANGES.some((preset) => preset.key === DEFAULT_ACTIVITY_RANGE));
  // A custom picker would have needed a key outside this list.
  assert.ok(!ACTIVITY_RANGES.some((preset) => /custom/i.test(preset.key)));
});

test("type breakdown keeps every known type, even at zero, and appends new ones", () => {
  const breakdown = typeBreakdown([
    { itemType: "rack", events: 5, items: 1 },
    { itemType: "miner", events: 40, items: 3 },
    { itemType: "battery", events: 0, items: 0 }
  ]);
  // The order is the fleet's static vocabulary, not the current volume, so the
  // header does not reshuffle while a filter narrows the table.
  assert.deepEqual(
    breakdown.map((total) => total.key),
    [...ITEM_TYPE_ORDER]
  );
  const miners = breakdown.find((total) => total.key === "miner");
  assert.equal(miners.events, 40);
  assert.equal(miners.items, 3);
  // A type with no frames still gets a badge; that keeps the component's shape
  // predictable instead of letting categories disappear.
  const batteries = breakdown.find((total) => total.key === "battery");
  assert.equal(batteries.events, 0);
  assert.equal(batteries.items, 0);
  // A type the feed introduces after this build is kept after the known list.
  const extra = typeBreakdown([{ itemType: "seed_pack", events: 4, items: 1 }]);
  assert.equal(extra.at(-1).key, "seed_pack");
  assert.equal(extra.at(-1).events, 4);
  // A malformed/missing total still yields the five known zero badges.
  assert.deepEqual(
    typeBreakdown(null).map((total) => total.key),
    [...ITEM_TYPE_ORDER]
  );
  assert.ok(
    typeBreakdown([{ itemType: "", events: 2, items: 1 }]).some(
      (total) => total.key === "unknown" && total.events === 2
    )
  );
});

test("the next batch cursor is the number of rows already held", () => {
  assert.deepEqual(nextBatch({ shown: 0, total: 72 }), {
    offset: 0,
    remaining: 72,
    hasMore: true
  });
  assert.deepEqual(nextBatch({ shown: 25, total: 72 }), {
    offset: 25,
    remaining: 47,
    hasMore: true
  });
  // The final batch leaves nothing behind.
  assert.deepEqual(nextBatch({ shown: 72, total: 72 }), {
    offset: 72,
    remaining: 0,
    hasMore: false
  });
  // A filter that shrinks the result set below what is on screen stops asking
  // for more instead of requesting an empty page forever.
  assert.deepEqual(nextBatch({ shown: 50, total: 12 }), {
    offset: 50,
    remaining: 0,
    hasMore: false
  });
  assert.deepEqual(nextBatch({}), { offset: 0, remaining: 0, hasMore: false });
  assert.deepEqual(nextBatch({ shown: NaN, total: -5 }), {
    offset: 0,
    remaining: 0,
    hasMore: false
  });
});
