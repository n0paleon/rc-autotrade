/**
 * Raw marketplace event payloads retained with loot rows for debugging.
 * Large frames are stored as an explicit preview so an unusually deep book
 * cannot turn the permanent loot log into an unbounded event archive.
 */

const MAX_EVENT_BYTES = 256 * 1024;
const MAX_PREVIEW_CHARS = 8 * 1024;

function snapshotPayload(value) {
  if (!value || typeof value !== "object") return null;
  try {
    const json = JSON.stringify(value);
    if (typeof json !== "string") return null;
    const byteLength = new TextEncoder().encode(json).byteLength;
    if (byteLength > MAX_EVENT_BYTES) {
      return {
        __truncatedForStorage: true,
        originalBytes: byteLength,
        preview: json.slice(0, MAX_PREVIEW_CHARS)
      };
    }
    return JSON.parse(json);
  } catch {
    return null;
  }
}

/** Keeps the market signal and optional result frame as JSON-safe snapshots. */
export function sanitizeRawEvents(raw) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const priceEvent = snapshotPayload(raw.priceEvent);
  const resultEvent = snapshotPayload(raw.resultEvent);
  if (!priceEvent && !resultEvent) return null;
  return { priceEvent, resultEvent };
}

/** Serializes raw event snapshots for the `orders.raw_events` TEXT column. */
export function encodeRawEvents(raw) {
  const events = sanitizeRawEvents(raw);
  return events ? JSON.stringify(events) : null;
}

/** Reads the TEXT column (or an already-parsed object) into safe snapshots. */
export function decodeRawEvents(value) {
  if (!value) return null;
  if (typeof value === "object") return sanitizeRawEvents(value);
  try {
    return sanitizeRawEvents(JSON.parse(value));
  } catch {
    return null;
  }
}
