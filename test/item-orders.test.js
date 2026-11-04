import test from "node:test";
import assert from "node:assert/strict";
import { RollercoinApi } from "../src/worker/api.js";

function makeApi(data) {
  const session = {
    calls: 0,
    getAccessToken: () => "test-access-token-123456",
    getCookie: () => "",
    async refresh() {
      this.calls += 1;
      return false;
    }
  };
  const fetchImpl = async () => ({
    status: 200,
    ok: true,
    text: async () => JSON.stringify({ success: true, error: "", data })
  });
  return new RollercoinApi({ baseUrl: "https://api.example.test", session, fetchImpl });
}

test("fetchItemOrders converts micro-RLT to RLT", async () => {
  const api = makeApi({ allowedPriceMin: 7_350_000, allowedPriceMax: 118_125_000, list: [], totalCount: 0 });
  const result = await api.fetchItemOrders({ itemId: "69e240f7459591770e4e27d5", itemType: "miner" });
  assert.equal(result.ok, true);
  assert.equal(result.data.allowedPriceMin, 7.35);
  assert.equal(result.data.allowedPriceMax, 118.125);
});

test("fetchItemOrders rejects a missing or inverted range", async () => {
  const inverted = await makeApi({ allowedPriceMin: 200_000_000, allowedPriceMax: 100_000_000 })
    .fetchItemOrders({ itemId: "abc", itemType: "miner" });
  assert.equal(inverted.ok, false);

  const missing = await makeApi({ allowedPriceMin: 1_000_000 }).fetchItemOrders({
    itemId: "abc",
    itemType: "miner"
  });
  assert.equal(missing.ok, false);
});

test("fetchItemOrders surfaces network failures", async () => {
  const api = new RollercoinApi({
    baseUrl: "https://api.example.test",
    session: { getAccessToken: () => "", getCookie: () => "", refresh: async () => false },
    fetchImpl: async () => {
      throw new Error("boom");
    }
  });
  const result = await api.fetchItemOrders({ itemId: "abc", itemType: "battery" });
  assert.equal(result.ok, false);
  assert.equal(result.status, 0);
});
