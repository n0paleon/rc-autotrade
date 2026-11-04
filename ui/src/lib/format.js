export const priceFmt = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 6,
  maximumFractionDigits: 6
});

export function fixedPrice(value) {
  const number = Number(value);
  return Number.isFinite(number) ? priceFmt.format(number) : "—";
}

const trimFmt = new Intl.NumberFormat("en-US", { maximumFractionDigits: 6 });

/** Same precision as fixedPrice, but trailing zeros are dropped. */
export function trimPrice(value) {
  const number = Number(value);
  return Number.isFinite(number) ? trimFmt.format(number) : "—";
}

export function formatRlt(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number.toFixed(6) : "0.000000";
}

export function formatSignedRlt(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return "0.000000";
  return `${number >= 0 ? "+" : "-"}${Math.abs(number).toFixed(6)}`;
}

export function formatInt(value) {
  const number = Number(value);
  return Number.isFinite(number) ? Math.max(0, Math.floor(number)).toLocaleString("en-US") : "0";
}

export function shortId(value) {
  return value ? `${String(value).slice(0, 6)}…${String(value).slice(-4)}` : "—";
}

export const ITEM_TYPE_LABELS = Object.freeze({
  miner: "miners",
  mutation_component: "parts",
  battery: "batteries",
  rack: "racks",
  utility_item: "other"
});

export const ITEM_TYPE_ORDER = Object.freeze(Object.keys(ITEM_TYPE_LABELS));

export function itemTypeOf(config) {
  return String(config?.itemType || config?.runtime?.itemType || "").toLowerCase();
}

export function itemTypeLabel(itemType) {
  const key = String(itemType || "").trim().toLowerCase();
  if (!key) return ITEM_TYPE_LABELS.utility_item;
  return ITEM_TYPE_LABELS[key] || key.replace(/_/g, " ");
}

export function marketplaceUrl(itemType, itemId) {
  const type = String(itemType || "mutation_component").toLowerCase();
  return `https://rollercoin.com/marketplace/buy/${encodeURIComponent(type)}/${encodeURIComponent(itemId)}`;
}

export function formatTime(ts) {
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "--:--:--";
  return date.toLocaleTimeString("en-GB", { hour12: false });
}

export function formatDate(ts) {
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleDateString("en-GB", { day: "2-digit", month: "short" });
}

export function formatUptime(ms) {
  const total = Math.max(0, Math.floor(ms / 1000));
  return [
    Math.floor(total / 3600),
    Math.floor((total % 3600) / 60),
    total % 60
  ]
    .map((value) => String(value).padStart(2, "0"))
    .join(":");
}

export function formatCountdown(ms) {
  if (!Number.isFinite(ms) || ms <= 0) return "EXPIRED";
  const total = Math.floor(ms / 1000);
  if (total >= 3600) {
    return `${Math.floor(total / 3600)}H ${String(Math.floor((total % 3600) / 60)).padStart(2, "0")}M`;
  }
  return `${String(Math.floor(total / 60)).padStart(2, "0")}:${String(total % 60).padStart(2, "0")}`;
}

export function relTime(ts) {
  if (!ts) return "—";
  const seconds = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (seconds < 3) return "JUST NOW";
  if (seconds < 60) return `${seconds}S AGO`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}M AGO`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}H AGO`;
  return `${Math.floor(seconds / 86_400)}D AGO`;
}

export function isRecent(ts, windowMs) {
  return Number.isFinite(ts) && Date.now() - ts < windowMs;
}

export function runtimeHot(config) {
  const runtime = config?.runtime || {};
  return Boolean(
    config?.active &&
      Number.isFinite(runtime.lastPrice) &&
      runtime.lastPrice <= config.targetPrice + 1e-9 &&
      Number.isFinite(runtime.lastStock) &&
      runtime.lastStock >= config.minAcceptableQty
  );
}

export function cardState(config) {
  if (!config.active) return "off";
  const runtime = config.runtime || {};
  if (runtime.inflight) return "buying";
  const last = runtime.lastResult;
  if (last && isRecent(last.at, 8000)) {
    if (last.status === "filled") return "filled";
    if (last.status === "rejected") return "error";
  }
  if (runtimeHot(config)) return "hot";
  return "idle";
}

export function stateLabel(state) {
  return (
    {
      off: "OFF",
      buying: "BUYING",
      filled: "FILLED",
      error: "ERROR",
      hot: "HOT",
      idle: "HUNT"
    }[state] || "HUNT"
  );
}

export function lastText(config) {
  const runtime = config.runtime || {};
  const last = runtime.lastResult;
  if (last && Number.isFinite(last.at)) {
    const age = Date.now() - last.at;
    if (last.status === "filled" && age < 60_000) {
      return `LOOT ${formatSignedRlt(last.profit)} · ${relTime(last.at)}`;
    }
    if (last.status === "rejected" && age < 60_000) {
      return `REJECTED · ${relTime(last.at)}`;
    }
    if (last.status === "dry" && age < 60_000) {
      return `DRY RUN ×${formatInt(last.qty)} · ${relTime(last.at)}`;
    }
    if ((last.status === "submitted" || last.status === "accepted") && age < 60_000) {
      return `${last.status.toUpperCase()} ×${formatInt(last.qty || 0)} · ${relTime(last.at)}`;
    }
  }
  if (Number.isFinite(runtime.lastSeenAt)) return `SEEN ${relTime(runtime.lastSeenAt)}`;
  return "WAITING FOR MARKET FRAME";
}

export function successStats(config) {
  const runtime = config?.runtime || {};
  const fills = Math.max(0, Number(runtime.fills) || 0);
  const failures = Math.max(0, Number(runtime.failures) || 0);
  const completed = fills + failures;
  return {
    fills,
    failures,
    completed,
    rate: completed > 0 ? Math.round((fills / completed) * 100) : null
  };
}

export function drawPixelAvatar(canvas, seed, preferredColor) {
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const palette = ["#b8ff3c", "#55e6ff", "#ff5fc8", "#a78bfa", "#ffc857"];
  let hash = 0x811c9dc5;
  for (let index = 0; index < String(seed).length; index += 1) {
    hash ^= String(seed).charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  hash >>>= 0;
  const color = preferredColor ? `#${preferredColor}` : palette[hash % palette.length];
  ctx.imageSmoothingEnabled = false;
  ctx.clearRect(0, 0, 8, 8);
  ctx.fillStyle = "#050910";
  ctx.fillRect(0, 0, 8, 8);
  for (let y = 0; y < 8; y += 1) {
    for (let x = 0; x < 4; x += 1) {
      const bit = (hash >>> ((x * 7 + y * 3) % 31)) & 1;
      const bit2 = (hash >>> ((x * 5 + y * 11 + 3) % 29)) & 1;
      if (bit ^ bit2) {
        ctx.fillStyle = color;
        ctx.fillRect(x, y, 1, 1);
        ctx.fillRect(7 - x, y, 1, 1);
      }
    }
  }
  ctx.fillStyle = "rgba(0,0,0,0.55)";
  ctx.fillRect(2, 3, 1, 1);
  ctx.fillRect(5, 3, 1, 1);
  ctx.fillStyle = "rgba(255,255,255,0.22)";
  ctx.fillRect(1, 1, 2, 1);
}

export function mergeHistory(history, point, bucket) {
  const next = Array.isArray(history) ? [...history] : [];
  const last = next[next.length - 1];
  if (!last || last.t < bucket) {
    next.push({ t: bucket, p: point });
    if (next.length > 1440) next.splice(0, next.length - 1440);
  } else {
    next[next.length - 1] = { ...last, p: point };
  }
  return next;
}

/* ── latency ───────────────────────────────────────────────────────── */

export function formatMs(value) {
  const number = Number(value);
  if (value === null || value === undefined || !Number.isFinite(number) || number < 0) return "—";
  if (number < 1000) return `${Math.round(number)}ms`;
  return `${(number / 1000).toFixed(2)}s`;
}

/** Color class for a latency value. */
export function latencyClass(value) {
  if (value === null || value === undefined) return "";
  const number = Number(value);
  if (!Number.isFinite(number)) return "";
  if (number <= 250) return "text-signal";
  if (number <= 800) return "text-ink-dim";
  if (number <= 2000) return "text-warn";
  return "text-danger";
}

/** Compact sparkline bars for the last N latency samples. */
export function sparkBars(values, count = 24) {
  const list = Array.isArray(values) ? values.slice(-count) : [];
  if (list.length === 0) return [];
  const max = Math.max(1, ...list);
  return list.map((value) => {
    const number = Number(value);
    const height = Number.isFinite(number) ? Math.max(12, Math.round((number / max) * 100)) : 12;
    return { value: number, height };
  });
}
