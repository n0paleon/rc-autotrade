import { test } from "bun:test";
import assert from "node:assert/strict";

import { countTargetsByStatus, filterTargets, targetIsActive } from "../src/lib/targets.js";

const fleet = [
  { itemId: "miner-1", label: "Aurora Miner", itemType: "miner", active: true },
  { itemId: "part-2", label: "Cooling Part", itemType: "mutation_component", active: false },
  { itemId: "rack-3", label: "Server Rack", itemType: "rack", active: true },
  { itemId: "rack-4", label: "Spare Rack", itemType: "rack" }
];

test("targetIsActive is strict about the arm flag", () => {
  assert.equal(targetIsActive({ active: true }), true);
  assert.equal(targetIsActive({ active: false }), false);
  assert.equal(targetIsActive({}), false);
  assert.equal(targetIsActive(null), false);
});

test("countTargetsByStatus splits the fleet into armed and parked", () => {
  assert.deepEqual(countTargetsByStatus(fleet), { all: 4, active: 2, inactive: 2 });
  assert.deepEqual(countTargetsByStatus([]), { all: 0, active: 0, inactive: 0 });
  assert.deepEqual(countTargetsByStatus(undefined), { all: 0, active: 0, inactive: 0 });
});

test("status filter keeps only the requested side of the arm switch", () => {
  assert.deepEqual(
    filterTargets(fleet, { statusFilter: "active" }).map((target) => target.itemId),
    ["miner-1", "rack-3"]
  );
  assert.deepEqual(
    filterTargets(fleet, { statusFilter: "inactive" }).map((target) => target.itemId),
    ["part-2", "rack-4"]
  );
  assert.equal(filterTargets(fleet, { statusFilter: "all" }).length, 4);
  // An unknown value must not hide the fleet.
  assert.equal(filterTargets(fleet, { statusFilter: "paused" }).length, 4);
});

test("status, type and search filters compose", () => {
  assert.deepEqual(
    filterTargets(fleet, { statusFilter: "active", typeFilter: "rack" }).map(
      (target) => target.itemId
    ),
    ["rack-3"]
  );
  assert.deepEqual(
    filterTargets(fleet, { statusFilter: "inactive", typeFilter: "rack" }).map(
      (target) => target.itemId
    ),
    ["rack-4"]
  );
  assert.deepEqual(
    filterTargets(fleet, { statusFilter: "active", search: "  AURORA " }).map(
      (target) => target.itemId
    ),
    ["miner-1"]
  );
  assert.deepEqual(filterTargets(fleet, { statusFilter: "inactive", search: "aurora" }), []);
});

test("search matches the item id and tolerates missing fields", () => {
  assert.deepEqual(
    filterTargets(fleet, { search: "PART-2" }).map((target) => target.itemId),
    ["part-2"]
  );
  assert.deepEqual(filterTargets([{ itemId: "x" }], { search: "" }).length, 1);
  assert.deepEqual(filterTargets([{}], { search: "x" }), []);
  assert.deepEqual(filterTargets(null, { statusFilter: "active" }), []);
});
