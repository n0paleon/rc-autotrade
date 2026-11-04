import { LIMITS } from "./constants.js";

const ITEM_ID_RE = /^[A-Za-z0-9_-]{6,64}$/;
const CONTROL_CHARS_RE = /[\u0000-\u001f\u007f]/g;

/**
 * Accepts a raw item id or a full Rollercoin marketplace URL and returns a
 * clean id. Returns null when the value cannot be a valid id.
 */
export function normalizeItemId(raw) {
  if (typeof raw !== "string") return null;
  let value = raw.trim();
  if (!value) return null;

  if (/^https?:\/\//i.test(value)) {
    try {
      const url = new URL(value);
      const parts = url.pathname.split("/").filter(Boolean);
      value = parts.at(-1) ?? "";
    } catch {
      return null;
    }
  }

  value = value.replace(CONTROL_CHARS_RE, "");
  return ITEM_ID_RE.test(value) ? value : null;
}

function toNumber(value) {
  if (typeof value === "number") return Number.isFinite(value) ? value : NaN;
  if (typeof value === "string" && value.trim() !== "") return Number(value);
  return NaN;
}

function hasDecimalsBeyond(value, digits) {
  const factor = 10 ** digits;
  return Math.abs(value * factor - Math.round(value * factor)) > 1e-6;
}

function boolOr(value, fallback) {
  if (typeof value === "boolean") return value;
  return fallback;
}

/**
 * Validates and normalises one target configuration.
 *
 * @param {unknown} input
 * @param {{existingIds?: string[], originalId?: string}} [opts]
 * @returns {{ok: true, value: object, errors: {}} | {ok: false, value: null, errors: Record<string,string>}}
 */
export function validateTarget(input, opts = {}) {
  const errors = {};
  const source = input && typeof input === "object" ? input : {};
  const existingIds = (opts.existingIds ?? []).map((id) => String(id).toLowerCase());
  const originalId = opts.originalId ? String(opts.originalId).toLowerCase() : null;

  const itemId = normalizeItemId(source.itemId);
  if (!itemId) {
    errors.itemId = "Item ID must be 6–64 letters, numbers, dashes or underscores (a marketplace URL also works).";
  } else if (existingIds.includes(itemId.toLowerCase()) && itemId.toLowerCase() !== originalId) {
    errors.itemId = "That item is already on the target list.";
  }

  const targetPrice = toNumber(source.targetPrice);
  if (!Number.isFinite(targetPrice) || targetPrice <= 0) {
    errors.targetPrice = "Target price must be greater than 0 RLT.";
  } else if (targetPrice > LIMITS.targetPriceMax) {
    errors.targetPrice = `Target price cannot exceed ${LIMITS.targetPriceMax.toLocaleString("en-US")} RLT.`;
  } else if (hasDecimalsBeyond(targetPrice, 6)) {
    errors.targetPrice = "Target price supports at most 6 decimal places.";
  }

  const desiredQty = toNumber(source.desiredQty);
  if (!Number.isInteger(desiredQty) || desiredQty < 1) {
    errors.desiredQty = "Desired quantity must be a whole number of at least 1.";
  } else if (desiredQty > LIMITS.qtyMax) {
    errors.desiredQty = `Desired quantity cannot exceed ${LIMITS.qtyMax.toLocaleString("en-US")}.`;
  }

  const highStockThreshold = toNumber(source.highStockThreshold);
  if (!Number.isInteger(highStockThreshold) || highStockThreshold < 1) {
    errors.highStockThreshold = "High-stock threshold must be a whole number of at least 1.";
  } else if (highStockThreshold > LIMITS.stockThresholdMax) {
    errors.highStockThreshold = `High-stock threshold cannot exceed ${LIMITS.stockThresholdMax.toLocaleString("en-US")}.`;
  } else if (Number.isInteger(desiredQty) && highStockThreshold < desiredQty) {
    errors.highStockThreshold = "High-stock threshold must be greater than or equal to desired quantity.";
  }

  const minAcceptableQty = toNumber(source.minAcceptableQty);
  if (!Number.isInteger(minAcceptableQty) || minAcceptableQty < 1) {
    errors.minAcceptableQty = "Minimum acceptable quantity must be a whole number of at least 1.";
  } else if (Number.isInteger(desiredQty) && minAcceptableQty > desiredQty) {
    errors.minAcceptableQty = "Minimum acceptable quantity cannot exceed desired quantity.";
  }

  let label = "";
  if (source.label !== undefined && source.label !== null) {
    if (typeof source.label !== "string") {
      errors.label = "Label must be text.";
    } else {
      label = source.label.replace(CONTROL_CHARS_RE, "").trim().slice(0, LIMITS.labelMax);
    }
  }

  let itemType = "mutation_component";
  if (source.itemType !== undefined && source.itemType !== null && source.itemType !== "") {
    if (typeof source.itemType !== "string" || !/^[A-Za-z][A-Za-z0-9_-]{1,39}$/.test(source.itemType.trim())) {
      errors.itemType = "Item type must be 2–40 letters, numbers, dashes or underscores.";
    } else {
      itemType = source.itemType.trim().toLowerCase();
    }
  }

  let rarityColor = null;
  if (source.rarityColor !== undefined && source.rarityColor !== null && source.rarityColor !== "") {
    if (typeof source.rarityColor !== "string" || !/^#?[0-9a-fA-F]{6}$/.test(source.rarityColor.trim())) {
      errors.rarityColor = "Rarity color must be a 6-digit hex color.";
    } else {
      rarityColor = source.rarityColor.trim().replace(/^#/, "").toLowerCase();
    }
  }

  // Allowed-price snapshot cache (from /api/marketplace/item-orders).
  // Informational only — an invalid/missing value means "not cached yet",
  // never a validation error, so old saves and plain edits keep working.
  const cachedPrice = (key) => {
    const value = toNumber(source[key]);
    if (!Number.isFinite(value) || value <= 0 || value > LIMITS.targetPriceMax) return null;
    return Math.round(value * 1e6) / 1e6;
  };
  const allowedPriceMin = cachedPrice("allowedPriceMin");
  const allowedPriceMax = cachedPrice("allowedPriceMax");
  const allowedPriceAt = (() => {
    const value = toNumber(source.allowedPriceAt);
    return Number.isFinite(value) && value > 0 ? Math.floor(value) : null;
  })();
  const hasAllowedPrice =
    allowedPriceMin !== null &&
    allowedPriceMax !== null &&
    allowedPriceMin <= allowedPriceMax &&
    allowedPriceAt !== null;

  if (Object.keys(errors).length > 0) {
    return { ok: false, value: null, errors };
  }

  return {
    ok: true,
    errors: {},
    value: {
      itemId,
      label,
      itemType,
      ...(rarityColor ? { rarityColor } : {}),
      ...(hasAllowedPrice ? { allowedPriceMin, allowedPriceMax, allowedPriceAt } : {}),
      targetPrice: Math.round(targetPrice * 1e6) / 1e6,
      desiredQty,
      highStockThreshold,
      minAcceptableQty,
      allowPartialOrder: boolOr(source.allowPartialOrder, true),
      active: boolOr(source.active, true)
    }
  };
}

export function validateTargets(input, opts = {}) {
  if (!Array.isArray(input)) {
    return { ok: false, errors: { configs: "Targets must be an array." }, values: [] };
  }
  if (input.length > LIMITS.configsMax) {
    return { ok: false, errors: { configs: `At most ${LIMITS.configsMax} targets are allowed.` }, values: [] };
  }

  const errors = {};
  const values = [];
  const seen = new Set();

  input.forEach((entry, index) => {
    const result = validateTarget(entry, {
      existingIds: [...(opts.existingIds ?? []), ...seen, ...values.map((v) => v.itemId)]
    });
    if (!result.ok) {
      for (const [field, message] of Object.entries(result.errors)) {
        errors[`${index}.${field}`] = message;
      }
      return;
    }
    seen.add(result.value.itemId.toLowerCase());
    values.push(result.value);
  });

  return { ok: Object.keys(errors).length === 0, errors, values };
}

export function validateSettings(input) {
  const source = input && typeof input === "object" ? input : {};
  const errors = {};
  const value = {};

  if (source.delayPerOrder !== undefined) {
    const delay = toNumber(source.delayPerOrder);
    if (!Number.isInteger(delay) || delay < LIMITS.delayMin || delay > LIMITS.delayMax) {
      errors.delayPerOrder = `Delay must be a whole number between ${LIMITS.delayMin} and ${LIMITS.delayMax} ms.`;
    } else {
      value.delayPerOrder = delay;
    }
  }

  if (source.maxOrdersPerMinute !== undefined) {
    const limit = toNumber(source.maxOrdersPerMinute);
    if (!Number.isInteger(limit) || limit < LIMITS.ordersPerMinuteMin || limit > LIMITS.ordersPerMinuteMax) {
      errors.maxOrdersPerMinute = `Order cap must be between ${LIMITS.ordersPerMinuteMin} and ${LIMITS.ordersPerMinuteMax} per minute.`;
    } else {
      value.maxOrdersPerMinute = limit;
    }
  }

  for (const key of ["dryRun", "autoRefresh", "paused"]) {
    if (source[key] !== undefined) {
      if (typeof source[key] !== "boolean") {
        errors[key] = `${key} must be true or false.`;
      } else {
        value[key] = source[key];
      }
    }
  }

  return { ok: Object.keys(errors).length === 0, errors, value };
}

const TOKEN_MIN = 8;
const TOKEN_MAX = 4096;

export function validateSessionInput(input, { allowEmptyRefresh = true } = {}) {
  const source = input && typeof input === "object" ? input : {};
  const errors = {};

  if (source.accessToken !== undefined) {
    if (typeof source.accessToken !== "string") {
      errors.accessToken = "Access token must be text.";
    } else {
      const token = source.accessToken.trim().replace(/^Bearer\s+/i, "");
      if (token.length < TOKEN_MIN) {
        errors.accessToken = "Access token looks too short.";
      } else if (token.length > TOKEN_MAX) {
        errors.accessToken = "Access token is too long.";
      }
    }
  }

  if (source.refreshToken !== undefined && source.refreshToken !== null) {
    if (typeof source.refreshToken !== "string") {
      errors.refreshToken = "Refresh token must be text.";
    } else {
      const token = source.refreshToken.trim();
      if (token && (token.length < TOKEN_MIN || token.length > TOKEN_MAX)) {
        errors.refreshToken = "Refresh token has an unexpected length.";
      } else if (!token && !allowEmptyRefresh) {
        errors.refreshToken = "A refresh token is required.";
      }
    }
  }

  if (source.cookie !== undefined && source.cookie !== null) {
    if (typeof source.cookie !== "string") {
      errors.cookie = "Cookie must be text.";
    } else if (source.cookie.length > 8192) {
      errors.cookie = "Cookie header is too long.";
    }
  }

  for (const key of ["clearAccessToken", "clearRefreshToken", "clearCookie"]) {
    if (source[key] !== undefined && typeof source[key] !== "boolean") {
      errors[key] = `${key} must be true or false.`;
    }
  }

  if (Object.keys(errors).length > 0) {
    return { ok: false, errors, value: null };
  }

  const value = {};
  if (typeof source.accessToken === "string") {
    value.accessToken = source.accessToken.trim().replace(/^Bearer\s+/i, "");
  }
  if (typeof source.refreshToken === "string") {
    value.refreshToken = source.refreshToken.trim();
  }
  if (typeof source.cookie === "string") {
    value.cookie = source.cookie.trim();
  }
  if (typeof source.source === "string") {
    value.source = source.source.slice(0, 40);
  }
  for (const key of ["clearAccessToken", "clearRefreshToken", "clearCookie"]) {
    if (source[key] === true) value[key] = true;
  }

  return { ok: true, errors: {}, value };
}
