/**
 * Normalises the handful of Rollercoin socket envelopes we care about.
 * The worker subscribes to every frame; this function turns the interesting
 * ones into a stable shape so the engine never has to guess.
 */

function firstDefined(...values) {
  for (const value of values) {
    if (value !== undefined && value !== null && value !== "") return value;
  }
  return undefined;
}

function asObject(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function toNumber(value) {
  if (typeof value === "number") return Number.isFinite(value) ? value : NaN;
  if (typeof value === "string" && value.trim() !== "") {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : NaN;
  }
  return NaN;
}

function parseListing(raw) {
  const source = asObject(raw);
  const priceRaw = toNumber(firstDefined(source.price, source.unit_price, source.unitPrice));
  const quantity = toNumber(
    firstDefined(source.quantity, source.count, source.qty, source.amount, source.total_count)
  );
  if (!Number.isFinite(priceRaw) || priceRaw <= 0) return null;
  if (!Number.isFinite(quantity) || quantity < 1) return null;
  return {
    priceRaw,
    quantity: Math.floor(quantity),
    unitPrice: priceRaw / 1_000_000
  };
}

function parseAllowedPrice(raw) {
  const value = toNumber(raw);
  if (!Number.isFinite(value) || value < 0) return null;
  return value / 1_000_000;
}

/**
 * @param {unknown} frame parsed socket frame
 * @returns {null | {
 *   cmd: string,
 *   itemId: string | null,
 *   currency: string | null,
 *   itemType: string | null,
 *   kind: "market" | "fill" | "other",
 *   allowedPriceMin: number | null,
 *   allowedPriceMax: number | null,
 *   listings: {priceRaw:number, quantity:number, unitPrice:number}[],
 *   fill: null | {orderId:string|null, count:number, priceRaw:number, unitPrice:number, status:number}
 * }}
 */
export function normalizeUpstreamEvent(frame) {
  const message = asObject(frame);
  const cmd = typeof message.cmd === "string" ? message.cmd : "";
  const value = asObject(message.value);
  const data = asObject(value.data);

  const rawItemId = firstDefined(
    value.item_id,
    value.itemId,
    data.item_id,
    data.itemId,
    asObject(value.item).id,
    asObject(value.item)._id,
    asObject(data.item).id
  );

  const itemId = rawItemId === undefined ? null : String(rawItemId).toLowerCase();
  const currencyRaw = firstDefined(value.currency, data.currency);
  const currency = currencyRaw === undefined ? null : String(currencyRaw).toUpperCase();
  const itemTypeRaw = firstDefined(value.item_type, value.itemType, data.item_type, data.itemType);

  const rawList = firstDefined(data.list, value.list, data.orders, value.orders);
  const listings = Array.isArray(rawList) ? rawList.map(parseListing).filter(Boolean) : [];
  if (listings.length > 0) listings.sort((a, b) => a.priceRaw - b.priceRaw || b.quantity - a.quantity);

  const allowedPriceMin = parseAllowedPrice(
    firstDefined(data.allowedPriceMin, value.allowedPriceMin, data.allowed_price_min, value.allowed_price_min)
  );
  const allowedPriceMax = parseAllowedPrice(
    firstDefined(data.allowedPriceMax, value.allowedPriceMax, data.allowed_price_max, value.allowed_price_max)
  );

  const orderIdRaw = firstDefined(value.order_id, value.orderId, data.order_id, data.orderId);
  const statusRaw = firstDefined(value.status, data.status);
  const status = toNumber(statusRaw);
  const count = toNumber(firstDefined(value.count, data.count, value.totalCount, data.total_count));
  const priceRaw = toNumber(firstDefined(value.price, data.price, value.totalPrice, data.total_price));

  const hasFillShape = orderIdRaw !== undefined && Number.isFinite(status);
  let kind = "other";
  if (listings.length > 0) kind = "market";
  else if (hasFillShape) kind = "fill";

  return {
    cmd,
    itemId,
    currency,
    itemType: itemTypeRaw === undefined ? null : String(itemTypeRaw),
    kind,
    allowedPriceMin,
    allowedPriceMax,
    listings,
    fill:
      kind === "fill"
        ? {
            orderId: orderIdRaw === undefined ? null : String(orderIdRaw),
            count: Number.isFinite(count) ? Math.floor(count) : 0,
            priceRaw: Number.isFinite(priceRaw) ? priceRaw : 0,
            unitPrice: Number.isFinite(priceRaw) && count > 0 ? priceRaw / 1_000_000 / count : 0,
            status
          }
        : null
  };
}
