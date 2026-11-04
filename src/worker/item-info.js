import { EventEmitter } from "node:events";

const DEFAULT_TTL_MS = 10 * 60 * 1000;
const FAILURE_COOLDOWN_MS = 5 * 60 * 1000;
const ITEM_TYPE_CANDIDATES = ["mutation_component", "miner", "rack", "battery", "utility_item"];

// Miner payloads carry no rarityGroup; rarity is encoded by data.level instead.
const MINER_LEVEL_RARITIES = ["", "Uncommon", "Rare", "Epic", "Legendary", "Unreal"];

// Color accent per miner rarity (same vibe as Rollercoin's title colors).
const MINER_LEVEL_COLORS = {
  Uncommon: "2bd600",
  Rare: "2196f3",
  Epic: "a52bff",
  Legendary: "ffb800",
  Unreal: "ff3b6b"
};

function isTypeMiss(result) {
  const message = String(result?.error || "").toLowerCase();
  return (
    (result?.status === 400 || result?.status === 404) &&
    (message.includes("item not found") || message.includes("item type is invalid"))
  );
}

function shortId(itemId) {
  return String(itemId || "?").slice(-6).toUpperCase();
}

export function normalizeItemInfo(data, itemId, fallbackType) {
  const clean = (value) =>
    String(value ?? "")
      .replace(/[\u0000-\u001f\u007f]/g, "")
      .replace(/\s+/g, " ")
      .trim();
  const rawName =
    typeof data?.name?.en === "string"
      ? data.name.en
      : typeof data?.name === "string"
        ? data.name
        : "";
  const itemType = String(data?.itemType || fallbackType || "mutation_component").toLowerCase();
  let rawRarity =
    typeof data?.rarityGroup?.title?.en === "string" ? data.rarityGroup.title.en : "";
  let rawColor = String(data?.rarityGroup?.baseHexColor ?? "").replace(/^#/, "");

  // Fallback: derive rarity from level for miners (no rarityGroup).
  if (!rawRarity && itemType === "miner") {
    const level = Number(data?.level);
    if (Number.isInteger(level) && level >= 0 && level < MINER_LEVEL_RARITIES.length) {
      const derived = MINER_LEVEL_RARITIES[level];
      if (derived) {
        rawRarity = derived;
        if (!/^[0-9a-fA-F]{6}$/.test(rawColor)) rawColor = MINER_LEVEL_COLORS[derived] || "";
      }
    }
  }

  const name = clean(rawName);
  const rarity = clean(rawRarity);
  const label = [rarity, name].filter(Boolean).join(" ").slice(0, 48) || `ITEM ${shortId(itemId)}`;
  const rarityColor = /^[0-9a-fA-F]{6}$/.test(rawColor) ? rawColor.toLowerCase() : null;

  return {
    itemId: String(itemId),
    itemName: name || null,
    rarity: rarity || null,
    rarityColor,
    itemType,
    label
  };
}

/**
 * Resolves marketplace item names through Rollercoin's item-info endpoint and
 * stores the composed `Rarity Name` label on the target configuration.
 */
export class ItemInfoService extends EventEmitter {
  constructor({ store, api, logger, now = () => Date.now(), ttlMs = DEFAULT_TTL_MS }) {
    super();
    this.store = store;
    this.api = api;
    this.logger = logger;
    this.now = now;
    this.ttlMs = ttlMs;
    this.cache = new Map();
    this.inflight = new Map();
    this.failedAt = new Map();
  }

  async #load(itemId, itemType, { fresh = false } = {}) {
    const key = `${String(itemId).toLowerCase()}:${itemType}`;
    const cached = this.cache.get(key);
    if (!fresh && cached && this.now() - cached.at < this.ttlMs) {
      return { ok: true, value: cached.value };
    }
    if (this.inflight.has(key)) return this.inflight.get(key);

    const promise = (async () => {
      const types = [
        itemType,
        ...ITEM_TYPE_CANDIDATES.filter((candidate) => candidate !== itemType)
      ];
      let lastResult = null;

      for (const candidateType of types) {
        const result = await this.api.fetchItemInfo({ itemId, itemType: candidateType });
        if (result.ok && result.data) {
          const value = normalizeItemInfo(result.data, itemId, candidateType);
          const timestamp = this.now();
          this.cache.set(`${String(itemId).toLowerCase()}:${candidateType}`, { at: timestamp, value });
          this.cache.set(key, { at: timestamp, value });
          this.failedAt.delete(key);
          return { ok: true, value };
        }

        lastResult = result;
        if (!isTypeMiss(result)) break;
      }

      const error = lastResult?.error || "item-info returned no data";
      const last = this.failedAt.get(key) || 0;
      if (this.now() - last > FAILURE_COOLDOWN_MS) {
        this.failedAt.set(key, this.now());
        this.logger?.warn("item-info", `could not resolve ${itemId}: ${error}`);
      }
      return { ok: false, error, status: lastResult?.status };
    })().finally(() => {
      this.inflight.delete(key);
    });

    this.inflight.set(key, promise);
    return promise;
  }

  async preview(itemId, itemType = "mutation_component") {
    return this.#load(itemId, itemType);
  }

  async enrich(itemId, itemType = "mutation_component", { fresh = false } = {}) {
    const result = await this.#load(itemId, itemType, { fresh });
    if (!result.ok) return result;

    const key = String(itemId).toLowerCase();
    const config = this.store.state.configs.find(
      (candidate) => candidate.itemId.toLowerCase() === key
    );
    if (!config) return result;

    const value = result.value;
    const changed =
      config.label !== value.label ||
      config.itemType !== value.itemType ||
      (config.rarityColor || null) !== (value.rarityColor || null);

    if (changed) {
      config.label = value.label;
      config.itemType = value.itemType;
      if (value.rarityColor) config.rarityColor = value.rarityColor;
      else delete config.rarityColor;
      this.store.save();
      this.logger?.info("item-info", `${config.itemId} => ${value.label}`);
      this.emit("updated", { itemId: config.itemId, item: value });
    }

    return result;
  }

  async enrichMany(configs, { fresh = false } = {}) {
    const queue = [...configs];
    const workers = Array.from({ length: Math.min(3, queue.length) }, async () => {
      while (queue.length > 0) {
        const config = queue.shift();
        if (!config?.itemId) continue;
        try {
          await this.enrich(config.itemId, config.itemType || "mutation_component", { fresh });
        } catch (error) {
          this.logger?.warn("item-info", `enrichment crashed: ${error?.message || error}`);
        }
      }
    });
    await Promise.all(workers);
  }
}
