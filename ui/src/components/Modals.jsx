import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { Icon, ModalShell, ToggleRow, useModalClose } from "./ModalShell.jsx";
import {
  applyTotalPriceMarkup,
  fixedPrice,
  normalizeItemId,
  relTime,
  shortId,
  trimPrice
} from "../lib/format.js";
import { tabKeyDown, useNow } from "../lib/hooks.js";
import { isQuoteFresh, quoteFor } from "../lib/quote.js";

export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.append(area);
      area.select();
      const ok = document.execCommand("copy");
      area.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

function FieldError({ id, message }) {
  if (!message) return null;
  return <em id={id} className="field-error" role="alert">{message}</em>;
}

/** Land the keyboard on the first field the form rejected, once it renders. */
function focusFirstInvalid(form) {
  requestAnimationFrame(() => {
    form?.querySelector('[aria-invalid="true"]')?.focus();
  });
}

/** Ids the worker can stream live quotes for: the same shape the API accepts. */
const WATCHABLE_ITEM_ID = /^[A-Za-z0-9_-]{6,64}$/;

function validateTargetLocally(target) {
  const errors = {};
  if (!target.itemId) {
    errors.itemId = "Paste an item ID or marketplace URL.";
  } else if (
    !/^https?:\/\//i.test(target.itemId) &&
    !/^[A-Za-z0-9_-]{6,64}$/.test(target.itemId)
  ) {
    errors.itemId = "Use a 6–64 character item ID or a full marketplace URL.";
  }
  if (!Number.isFinite(target.targetPrice) || target.targetPrice <= 0) {
    errors.targetPrice = "Enter a price greater than 0.";
  }
  if (!Number.isInteger(target.desiredQty) || target.desiredQty < 1) {
    errors.desiredQty = "Enter a whole number of at least 1.";
  }
  if (!Number.isInteger(target.highStockThreshold) || target.highStockThreshold < 1) {
    errors.highStockThreshold = "Enter a threshold of at least 1.";
  } else if (Number.isInteger(target.desiredQty) && target.highStockThreshold < target.desiredQty) {
    errors.highStockThreshold = "Threshold must be at least the desired qty.";
  }
  if (!Number.isInteger(target.minAcceptableQty) || target.minAcceptableQty < 1) {
    errors.minAcceptableQty = "Enter a minimum of at least 1.";
  } else if (Number.isInteger(target.desiredQty) && target.minAcceptableQty > target.desiredQty) {
    errors.minAcceptableQty = "Minimum cannot exceed desired qty.";
  }
  return errors;
}

/**
 * Initial value of one form field. An existing target always wins; otherwise a
 * bootstrap draft from item activity can supply a starting value. Anything
 * missing stays empty, which is the normal "Deploy target" experience.
 */
function initialField(config, prefill, key) {
  if (config) return String(config[key] ?? "");
  const value = prefill?.[key];
  return value === null || value === undefined || value === "" ? "" : String(value);
}

export function TargetModal({ config, onClose, worker, notify, markupPct = 0, prefill = null }) {
  const { closeRef, close } = useModalClose();
  const editing = Boolean(config);
  const [itemId, setItemId] = useState(config?.itemId || prefill?.itemId || "");
  const [itemType, setItemType] = useState(config?.itemType || prefill?.itemType || "mutation_component");
  const [targetPrice, setTargetPrice] = useState(initialField(config, prefill, "targetPrice"));
  const [desiredQty, setDesiredQty] = useState(initialField(config, prefill, "desiredQty"));
  const [highStockThreshold, setHighStockThreshold] = useState(
    initialField(config, prefill, "highStockThreshold")
  );
  const [minAcceptableQty, setMinAcceptableQty] = useState(
    initialField(config, prefill, "minAcceptableQty")
  );
  const [allowPartialOrder, setAllowPartialOrder] = useState(config?.allowPartialOrder !== false);
  const [active, setActive] = useState(config?.active !== false);
  const [errors, setErrors] = useState({});
  const [busy, setBusy] = useState(false);
  const [auto, setAuto] = useState(() => {
    if (config) {
      return {
        state: config.label ? "ready" : "loading",
        label: config.label || "Resolving item name…",
        rarity: "",
        color: ""
      };
    }
    if (prefill?.label) {
      // The name is already known from item activity, so show it immediately.
      // The usual preview lookup still runs below to fill in rarity and colour.
      return { state: "ready", label: prefill.label, rarity: "", color: "" };
    }
    return { state: "idle", label: "Waiting for an item ID…", rarity: "", color: "" };
  });
  const lookupSeq = useRef(0);
  const lookupRef = useRef(null);
  const editLookupDone = useRef(false);

  // The id the worker matches market frames against. Input may be a raw id or
  // a marketplace URL; both must resolve to the same live-quote key.
  const lookupId = useMemo(() => normalizeItemId(itemId), [itemId]);
  const now = useNow(1000);
  const liveQuote = quoteFor(worker?.quotes, lookupId);
  const quoteLive = isQuoteFresh(liveQuote, now);
  const runtime = config?.runtime || {};

  const lookup = useCallback(async () => {
    const raw = itemId.trim();
    if (!raw) {
      setAuto({ state: "idle", label: "Waiting for an item ID…", rarity: "", color: "" });
      return;
    }
    const seq = ++lookupSeq.current;
    setAuto((previous) => ({ ...previous, state: "loading", label: "LOOKING UP ITEM…" }));
    try {
      const result = await worker.previewItem(raw, itemType || "mutation_component");
      if (seq !== lookupSeq.current) return;
      const item = result.item;
      setItemType(item.itemType || itemType || "mutation_component");
      setAuto({
        state: "ready",
        label: item.label || "Unknown item",
        rarity: item.rarity || "",
        color: item.rarityColor || ""
      });
    } catch (error) {
      if (seq !== lookupSeq.current || error.status === 401) return;
      const message = String(error?.message || "");
      let text = "Auto-label failed — retry or save to try again.";
      if (error.status === 404) text = "Auto-label endpoint unavailable — restart worker.";
      else if (/item not found/i.test(message)) text = "Item not found — check the ID/item type.";
      else if (error.status === 0) text = "Worker offline — retry when it is back.";
      else if (message) text = message.slice(0, 72);
      setAuto({ state: "warn", label: text, rarity: "", color: "" });
    }
  }, [itemId, itemType, worker]);

  lookupRef.current = lookup;

  useEffect(() => {
    if (!editing) return;
    if (!config?.label && !editLookupDone.current) {
      editLookupDone.current = true;
      lookupRef.current?.();
    }
  }, [editing, config]);

  useEffect(() => {
    if (editing) return;
    if (!itemId.trim()) {
      setAuto({ state: "idle", label: "Waiting for an item ID…", rarity: "", color: "" });
      return undefined;
    }
    const timer = setTimeout(() => lookupRef.current?.(), 450);
    return () => clearTimeout(timer);
  }, [itemId, editing]);

  // Watch the current item so the worker streams a compact quote even when the
  // id is not on the target fleet yet. A short debounce keeps typing from
  // subscribing to every partial id; the cleanup always releases the key.
  const watchItem = worker?.watchItem;
  const unwatchItem = worker?.unwatchItem;
  useEffect(() => {
    if (!WATCHABLE_ITEM_ID.test(lookupId)) return undefined;
    const key = lookupId.toLowerCase();
    const timer = setTimeout(() => watchItem?.(key), editing ? 0 : 350);
    return () => {
      clearTimeout(timer);
      unwatchItem?.(key);
    };
  }, [editing, lookupId, unwatchItem, watchItem]);

  // Before the first watched frame arrives, an existing target already has a
  // hydrated runtime. It is a snapshot, not live, so the UI labels it cached.
  const marketPrice = Number.isFinite(liveQuote?.price)
    ? liveQuote.price
    : Number.isFinite(runtime.lastPrice)
      ? runtime.lastPrice
      : null;
  const minPrice = Number.isFinite(liveQuote?.allowedPriceMin)
    ? liveQuote.allowedPriceMin
    : Number.isFinite(runtime.allowedPriceMin)
      ? runtime.allowedPriceMin
      : null;
  const maxPrice = Number.isFinite(liveQuote?.allowedPriceMax)
    ? liveQuote.allowedPriceMax
    : Number.isFinite(runtime.allowedPriceMax)
      ? runtime.allowedPriceMax
      : null;
  const hasQuoteFallback = marketPrice !== null || minPrice !== null || maxPrice !== null;
  const quoteState = !lookupId
    ? "idle"
    : liveQuote
      ? quoteLive
        ? "live"
        : "stale"
      : hasQuoteFallback
        ? "cached"
        : "waiting";
  const quoteStateText =
    quoteState === "live"
      ? "Live"
      : quoteState === "stale"
        ? `Stale · ${relTime(liveQuote.receivedAt)}`
        : quoteState === "cached"
          ? "Cached snapshot"
          : quoteState === "waiting"
            ? worker?.status === "online"
              ? "Waiting for frame"
              : "Worker offline"
            : "No item ID";
  const targetNumber = Number(targetPrice);
  const inRange =
    Number.isFinite(marketPrice) &&
    Number.isFinite(targetNumber) &&
    targetNumber > 0 &&
    marketPrice <= targetNumber;

  const maxSpend = useMemo(() => {
    const price = Number(targetPrice) || 0;
    const qty = Number(desiredQty) || 0;
    if (!(price > 0 && qty > 0)) return "—";
    // Worst case: price at the target ceiling, high-stock doubling active, and
    // the marketplace taking the whole total-price headroom.
    const ceiling = applyTotalPriceMarkup(price * qty * 2, markupPct);
    return `${fixedPrice(ceiling)} RLT`;
  }, [targetPrice, desiredQty, markupPct]);

  const submit = async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const target = {
      itemId: itemId.trim(),
      itemType: itemType || "mutation_component",
      targetPrice: Number(targetPrice),
      desiredQty: Number(desiredQty),
      highStockThreshold: Number(highStockThreshold),
      minAcceptableQty: Number(minAcceptableQty),
      allowPartialOrder,
      active
    };
    const localErrors = validateTargetLocally(target);
    setErrors(localErrors);
    if (Object.keys(localErrors).length > 0) {
      focusFirstInvalid(form);
      return;
    }

    setBusy(true);
    // `saveTarget` writes the optimistic config before it awaits the request.
    // Close the dialog right away so the updated card is what the operator sees;
    // a rejection rolls the card back and reports the reason as a toast.
    const saving = worker.saveTarget(target, config?.itemId);
    close();
    try {
      await saving;
      // The auto label may still be resolving on a fresh deploy, so fall back
      // to the short id instead of announcing "Waiting for an item ID…".
      const savedLabel = auto.state === "ready" && auto.label ? auto.label : shortId(target.itemId);
      notify("success", editing ? "TARGET UPDATED" : "TARGET DEPLOYED", savedLabel);
    } catch (error) {
      const details = Object.entries(error?.fields || {})
        .map(([field, message]) => `${field}: ${message}`)
        .join(" · ");
      notify(
        "error",
        details ? "CHECK THE FIELDS" : "SAVE FAILED",
        details || error?.message || "The worker could not save this target."
      );
    }
  };

  return (
    <ModalShell
      title={editing ? "Edit target" : "Deploy target"}
      onClose={onClose}
      closeRef={closeRef}
      footer={
        <div className="flex flex-col gap-2 sm:flex-row sm:justify-end">
          <button className="btn" type="button" onClick={close}>
            Cancel
          </button>
          <button className="btn btn-primary" type="submit" form="target-form" disabled={busy} data-busy={busy ? "true" : undefined} aria-busy={busy}>
            {busy ? (editing ? "Saving…" : "Deploying…") : editing ? "Save target" : "Deploy target"}
          </button>
        </div>
      }
    >
      <form id="target-form" className="modal-body" noValidate autoComplete="off" onSubmit={submit}>
        <div className="grid gap-4 sm:grid-cols-2">
          <label className="field sm:col-span-2">
            <span>Item ID / marketplace URL</span>
            <input
              className={`input${errors.itemId ? " border-danger/60" : ""}`}
              name="itemId"
              value={itemId}
              onChange={(event) => setItemId(event.target.value)}
              placeholder="Paste item ID or marketplace URL"
              readOnly={editing}
              autoComplete="off"
              autoCapitalize="none"
              spellCheck="false"
              aria-invalid={Boolean(errors.itemId)}
              aria-describedby={errors.itemId ? "target-itemId-error" : undefined}
              required
            />
            <small>
              {editing
                ? "Item ID is fixed after deployment. Delete the target to point this slot at a different item."
                : "Paste the ID or the full Rollercoin marketplace URL. The label is resolved automatically."}
            </small>
            <FieldError id="target-itemId-error" message={errors.itemId} />
          </label>

          <div className="auto-label sm:col-span-2" data-auto-label data-state={auto.state}>
            <div className="flex items-center justify-between gap-2">
              <span className="label">Auto label</span>
              <span className="flex items-center gap-2">
                {auto.rarity ? (
                  <span
                    className="border-2 border-[#05030f] px-1.5 py-0.5 font-mono text-[12px] uppercase tracking-[0.08em]"
                    style={auto.color ? { borderColor: `#${auto.color}`, color: `#${auto.color}` } : undefined}
                  >
                    {auto.rarity}
                  </span>
                ) : null}
                {auto.state === "warn" ? (
                  <button
                    className="tap-pad inline-flex min-h-6 items-center px-1 font-mono text-[13px] uppercase tracking-[0.1em] text-gold underline-offset-2 hover:underline"
                    type="button"
                    onClick={() => lookupRef.current?.()}
                  >
                    Retry
                  </button>
                ) : null}
              </span>
            </div>
            <strong className="mt-1 block text-[14px] font-medium text-ink">{auto.label}</strong>
            <small className="mt-0.5 block text-meta leading-snug text-faint">
              From Rollercoin item-info: rarity + item name. Falls back to the item name when rarity is absent.
            </small>
          </div>

          <div
            className={`@container sm:col-span-2 border-2 bg-night-950/50 px-3 py-2.5 ${
              quoteState === "live"
                ? "border-signal/50"
                : quoteState === "stale"
                  ? "border-gold/50"
                  : "border-line"
            }`}
            aria-label="Live market prices for this item"
          >
            <div className="flex items-center justify-between gap-2">
              <span className="label">Live market</span>
              <span
                className={`flex items-center gap-1.5 font-mono text-micro uppercase tracking-[0.08em] ${
                  quoteState === "live"
                    ? "text-signal"
                    : quoteState === "stale"
                      ? "text-gold"
                      : "text-faint"
                }`}
                role="status"
              >
                <span
                  className={`led ${
                    quoteState === "live"
                      ? "led-pulse bg-signal"
                      : quoteState === "stale"
                        ? "bg-gold"
                        : quoteState === "cached"
                          ? "bg-info"
                          : "bg-faint"
                  }`}
                  aria-hidden="true"
                />
                {quoteStateText}
              </span>
            </div>
            {/* Three columns only when the panel is wide enough for a band
                ceiling like "1,000,000"; narrower panels turn each stat into
                its own label/value row so the figure never spills the border. */}
            <dl className="mt-2 grid grid-cols-1 gap-[2px] border-2 border-line bg-line @[24rem]:grid-cols-3">
              <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-2 bg-night-900/70 px-2 py-1.5 @[24rem]:block">
                <dt className="label" title="Live best ask from the item's newest market frame">
                  Market price
                </dt>
                <dd
                  className={`flex min-w-0 flex-wrap items-baseline gap-x-1 font-mono text-[15px] font-bold leading-none tabular-nums @[24rem]:mt-0.5 ${
                    inRange ? "text-signal" : "text-info"
                  }`}
                >
                  <span className="min-w-0 [overflow-wrap:anywhere]">{trimPrice(marketPrice)}</span>
                  <span className="text-[11px] font-normal text-faint">RLT</span>
                </dd>
              </div>
              <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-2 bg-night-900/70 px-2 py-1.5 @[24rem]:block">
                <dt className="label" title="Lowest allowed listing price from the latest live frame">
                  Min price
                </dt>
                <dd className="flex min-w-0 flex-wrap items-baseline gap-x-1 font-mono text-[15px] font-bold leading-none tabular-nums text-info @[24rem]:mt-0.5">
                  <span className="min-w-0 [overflow-wrap:anywhere]">{trimPrice(minPrice)}</span>
                  <span className="text-[11px] font-normal text-faint">RLT</span>
                </dd>
              </div>
              <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-2 bg-night-900/70 px-2 py-1.5 @[24rem]:block">
                <dt className="label" title="Highest allowed listing price from the latest live frame">
                  Max price
                </dt>
                <dd className="flex min-w-0 flex-wrap items-baseline gap-x-1 font-mono text-[15px] font-bold leading-none tabular-nums text-pink @[24rem]:mt-0.5">
                  <span className="min-w-0 [overflow-wrap:anywhere]">{trimPrice(maxPrice)}</span>
                  <span className="text-[11px] font-normal text-faint">RLT</span>
                </dd>
              </div>
            </dl>
            <small className="mt-1.5 block text-meta leading-snug text-faint">
              Best ask and the marketplace&apos;s allowed price band, pushed live by the worker&apos;s market feed
              for this item.
            </small>
          </div>

          <label className="field">
            <span>Target price (RLT)</span>
            <input
              className={`input${errors.targetPrice ? " border-danger/60" : ""}`}
              name="targetPrice"
              value={targetPrice}
              onChange={(event) => setTargetPrice(event.target.value)}
              type="number"
              inputMode="decimal"
              step="0.000001"
              min="0.000001"
              placeholder="e.g. 0.100000"
              aria-invalid={Boolean(errors.targetPrice)}
              aria-describedby={errors.targetPrice ? "target-price-error" : undefined}
              required
            />
            <small>Never pay more than this per item. The engine buys only listings at or below it.</small>
            <FieldError id="target-price-error" message={errors.targetPrice} />
          </label>
          <label className="field">
            <span>Desired qty</span>
            <input
              className={`input${errors.desiredQty ? " border-danger/60" : ""}`}
              name="desiredQty"
              value={desiredQty}
              onChange={(event) => setDesiredQty(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              max="10000"
              placeholder="e.g. 10"
              aria-invalid={Boolean(errors.desiredQty)}
              aria-describedby={errors.desiredQty ? "target-qty-error" : undefined}
              required
            />
            <FieldError id="target-qty-error" message={errors.desiredQty} />
          </label>
          <label className="field">
            <span>High stock threshold</span>
            <input
              className={`input${errors.highStockThreshold ? " border-danger/60" : ""}`}
              name="highStockThreshold"
              value={highStockThreshold}
              onChange={(event) => setHighStockThreshold(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              placeholder="e.g. 20"
              aria-invalid={Boolean(errors.highStockThreshold)}
              aria-describedby={errors.highStockThreshold ? "target-high-error" : undefined}
              required
            />
            <small>At or above this stock, order qty doubles.</small>
            <FieldError id="target-high-error" message={errors.highStockThreshold} />
          </label>
          <label className="field">
            <span>Min acceptable qty</span>
            <input
              className={`input${errors.minAcceptableQty ? " border-danger/60" : ""}`}
              name="minAcceptableQty"
              value={minAcceptableQty}
              onChange={(event) => setMinAcceptableQty(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              placeholder="e.g. 1"
              aria-invalid={Boolean(errors.minAcceptableQty)}
              aria-describedby={errors.minAcceptableQty ? "target-min-error" : undefined}
              required
            />
            <small>Partial fills below this are skipped.</small>
            <FieldError id="target-min-error" message={errors.minAcceptableQty} />
          </label>
          <div className="field">
            <span>Max spend per fill</span>
            <output className="output-box">{maxSpend}</output>
            <small>
              Target × doubled quantity ceiling
              {Number(markupPct) > 0 ? `, including the ${Number(markupPct)}% total-price markup` : ""}.
            </small>
          </div>
        </div>

        <div className="space-y-2">
          <ToggleRow
            checked={allowPartialOrder}
            onChange={setAllowPartialOrder}
            title="Allow partial order"
            hint="Buy whatever is available above the minimum."
          />
          <ToggleRow
            checked={active}
            onChange={setActive}
            title="Arm target"
            hint="Start hunting as soon as it is saved."
          />
        </div>
      </form>
    </ModalShell>
  );
}

export function SessionModal({ session, onClose, worker, notify }) {
  const { closeRef, close } = useModalClose();
  const [accessToken, setAccessToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [cookie, setCookie] = useState("");
  const [clearCookie, setClearCookie] = useState(false);
  const [errors, setErrors] = useState({});
  const [formError, setFormError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    setFormError("");
    const body = {};
    if (accessToken.trim()) body.accessToken = accessToken.trim();
    if (refreshToken.trim()) body.refreshToken = refreshToken.trim();
    if (cookie.trim()) body.cookie = cookie.trim();
    if (clearCookie) body.clearCookie = true;
    if (!session.hasAccessToken && !body.accessToken && !clearCookie) {
      setErrors({ accessToken: "Paste an access token first." });
      focusFirstInvalid(form);
      return;
    }
    if (Object.keys(body).length === 0) {
      setErrors({ accessToken: "Nothing to update — paste a token or clear the form." });
      focusFirstInvalid(form);
      return;
    }
    setBusy(true);
    try {
      await worker.saveSession(body);
      notify("success", "SESSION SAVED", "The worker is re-linking to the market feed.");
      close();
    } catch (error) {
      if (error.fields && Object.keys(error.fields).length > 0) {
        setErrors(error.fields);
        focusFirstInvalid(form);
      } else {
        setFormError(error.message || "The worker could not save this session.");
      }
      setBusy(false);
    }
  };

  return (
    <ModalShell
      title="Link Rollercoin session"
      onClose={onClose}
      closeRef={closeRef}
      footer={
        <div className="flex flex-col gap-2 sm:flex-row sm:justify-end">
          <button className="btn" type="button" onClick={close}>
            Cancel
          </button>
          <button className="btn btn-primary" type="submit" form="session-form" disabled={busy} data-busy={busy ? "true" : undefined} aria-busy={busy}>
            {busy ? "Saving…" : "Save session"}
          </button>
        </div>
      }
    >
      <form id="session-form" className="modal-body" noValidate autoComplete="off" onSubmit={submit}>
        <p className="text-copy text-muted">
          Grab the tokens from a logged-in rollercoin.com tab: DevTools → Application → Local Storage →{" "}
          <code className="text-ink-dim">token</code> and <code className="text-ink-dim">refreshToken</code>. The
          browser bridge can keep them in sync automatically.
        </p>
        {formError ? (
          <p className="flex items-start gap-2 border-2 border-danger/50 bg-danger/5 p-2.5 text-body leading-relaxed text-danger" role="alert">
            <Icon name="i-alert" className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{formError}</span>
          </p>
        ) : null}
        <label className="field">
          <span>Access token</span>
          <textarea
            className="input"
            rows="3"
            value={accessToken}
            onChange={(event) => setAccessToken(event.target.value)}
            spellCheck="false"
            aria-invalid={Boolean(errors.accessToken)}
            aria-describedby={errors.accessToken ? "session-access-error" : undefined}
            placeholder={
              session.hasAccessToken
                ? `Current ${session.accessPreview || "token"} — leave blank to keep`
                : "Paste the Rollercoin access token"
            }
          />
          <FieldError id="session-access-error" message={errors.accessToken} />
        </label>
        <label className="field">
          <span>Refresh token</span>
          <textarea
            className="input"
            rows="2"
            value={refreshToken}
            onChange={(event) => setRefreshToken(event.target.value)}
            spellCheck="false"
            aria-invalid={Boolean(errors.refreshToken)}
            aria-describedby={errors.refreshToken ? "session-refresh-error" : undefined}
            placeholder={
              session.hasRefreshToken
                ? `Current ${session.refreshPreview || "token"} — leave blank to keep`
                : "Paste the refresh token"
            }
          />
          <FieldError id="session-refresh-error" message={errors.refreshToken} />
        </label>
        <label className="field">
          <span>Cloudflare cookie (optional)</span>
          <textarea
            className="input"
            rows="2"
            value={cookie}
            onChange={(event) => setCookie(event.target.value)}
            spellCheck="false"
            aria-invalid={Boolean(errors.cookie)}
            aria-describedby={errors.cookie ? "session-cookie-error" : undefined}
            placeholder={
              session.hasCookie
                ? "A cookie is stored — leave blank to keep it"
                : "cf_clearance=…; __cf_bm=… (optional)"
            }
          />
          <small>Only needed if Rollercoin rejects API calls with a Cloudflare challenge.</small>
          <FieldError id="session-cookie-error" message={errors.cookie} />
        </label>
        {session.hasCookie ? (
          <ToggleRow
            checked={clearCookie}
            onChange={setClearCookie}
            title="Clear stored cookie"
            hint="Remove the saved Cloudflare cookie without touching the tokens."
          />
        ) : null}
      </form>
    </ModalShell>
  );
}

function buildUserscript(code, baseUrl) {
  let host = "127.0.0.1";
  try {
    host = new URL(baseUrl).hostname;
  } catch {
    /* keep loopback */
  }
  const connects = [...new Set([host, "127.0.0.1", "localhost"])].filter(Boolean);
  return [
    "// ==UserScript==",
    "// @name         TURU.RC Token Bridge",
    "// @namespace    turu.rc",
    "// @version      1.0.0",
    "// @description  Keep a local TURU.RC worker supplied with Rollercoin session tokens.",
    "// @author       TURU.RC",
    "// @match        https://rollercoin.com/*",
    "// @match        https://*.rollercoin.com/*",
    "// @run-at       document-idle",
    "// @noframes",
    "// @grant        GM_xmlhttpRequest",
    "// @grant        unsafeWindow",
    ...connects.map((value) => `// @connect      ${value}`),
    "// ==/UserScript==",
    "",
    code
  ].join("\n");
}

export function BridgeModal({ onClose, getConnection, notify }) {
  const { closeRef, close } = useModalClose();
  const [mode, setMode] = useState("console");
  const [snippet, setSnippet] = useState("");
  const [userscript, setUserscript] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    let cancelled = false;
    const { baseUrl, token } = getConnection();
    fetch(`${baseUrl}/bridge.js`, { cache: "no-store" })
      .then((response) => {
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.text();
      })
      .then((template) => {
        if (cancelled) return;
        const code = template
          .split("__WORKER_URL__")
          .join(baseUrl)
          .split("__WORKER_TOKEN__")
          .join(token);
        setSnippet(code);
        setUserscript(buildUserscript(code, baseUrl));
      })
      .catch(() => {
        if (!cancelled) setError("Could not load /bridge.js from the worker.");
      });
    return () => {
      cancelled = true;
    };
  }, [getConnection]);

  const code = mode === "console" ? snippet : userscript;

  const copy = async () => {
    if (!code) {
      notify("warn", "NOT READY", "The bridge is still loading.");
      return;
    }
    const ok = await copyText(code);
    notify(
      ok ? "info" : "error",
      ok ? (mode === "console" ? "SNIPPET COPIED" : "USERSCRIPT COPIED") : "COPY FAILED",
      mode === "console" ? "Paste it in the Rollercoin console." : "Paste it into Tampermonkey and save."
    );
  };

  const download = () => {
    if (!userscript) {
      notify("warn", "NOT READY", "The userscript is still loading.");
      return;
    }
    try {
      const blob = new Blob([userscript], { type: "text/javascript" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = "turu-rc-token-bridge.user.js";
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 15_000);
      notify("success", "USERSCRIPT DOWNLOADED", "Open it in Tampermonkey, or copy the code manually.");
    } catch {
      notify("error", "DOWNLOAD FAILED", "Use COPY USERSCRIPT instead.");
    }
  };

  return (
    <ModalShell title="Token bridge" onClose={onClose} closeRef={closeRef} wide>
      <div className="modal-body">
        <div className="tab-rail grid-cols-2" role="tablist" aria-label="Bridge mode">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "console"}
            tabIndex={mode === "console" ? 0 : -1}
            className="pixel-tab"
            onClick={() => setMode("console")}
            onKeyDown={tabKeyDown}
          >
            Console snippet
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "userscript"}
            tabIndex={mode === "userscript" ? 0 : -1}
            className="pixel-tab"
            onClick={() => setMode("userscript")}
            onKeyDown={tabKeyDown}
          >
            Userscript · auto
          </button>
        </div>

        {mode === "console" ? (
          <ol className="steps">
            <li><strong>Open rollercoin.com</strong> in a logged-in tab.</li>
            <li>Open DevTools → Console, paste the snippet below, press Enter.</li>
            <li>It pushes your token every 30 s and writes refreshed tokens back to the site.</li>
          </ol>
        ) : (
          <ol className="steps">
            <li><strong>Install Tampermonkey</strong> (or Violentmonkey) in your browser, once.</li>
            <li>Copy or download the userscript below, then paste it into a new Tampermonkey script and save.</li>
            <li>Open rollercoin.com while logged in. Allow the cross-origin request if Tampermonkey asks. The bridge starts automatically and restarts on every reload.</li>
            <li>Leave the Rollercoin tab open (it can be inactive). The worker keeps buying even when the tab is throttled.</li>
          </ol>
        )}

        <div className="space-y-2">
          <pre className="code-block" tabIndex={0} aria-label="Bridge code"><code>{code || (error || "Loading bridge…")}</code></pre>
          <div className="flex flex-wrap gap-2">
            <button className="btn btn-primary btn-sm" type="button" onClick={copy}>
              <Icon name="i-copy" className="h-3 w-3" />
              {mode === "console" ? "Copy snippet" : "Copy userscript"}
            </button>
            {mode === "userscript" ? (
              <button className="btn btn-ghost btn-sm" type="button" onClick={download}>
                <Icon name="i-download" className="h-3 w-3" />
                Download .user.js
              </button>
            ) : null}
          </div>
        </div>

        {mode === "console" ? (
          <p className="flex items-start gap-2 text-copy text-faint">
            <Icon name="i-alert" className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />
            <span>
              Console mode uses the page's <code className="text-ink-dim">fetch</code>, so Rollercoin's CSP or
              browser privacy rules can block localhost with a CORS error. If that happens, switch to
              Userscript · auto. It also must be pasted again after a reload.
            </span>
          </p>
        ) : (
          <p className="flex items-start gap-2 text-copy text-faint">
            <Icon name="i-check" className="mt-0.5 h-3.5 w-3.5 shrink-0 text-signal" />
            <span>
              Userscript mode uses <code className="text-ink-dim">GM_xmlhttpRequest</code>, which bypasses page CORS
              and CSP. Install it once; after that you only need the worker running and a logged-in Rollercoin tab.
              Regenerate it if you rotate the access key.
            </span>
          </p>
        )}
        <p className="flex items-start gap-2 text-copy text-faint">
          <Icon name="i-alert" className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />
          <span>
            Both variants contain this deck's access key. Keep them private and never paste them into a public gist
            or chat.
          </span>
        </p>
        <div className="flex justify-end">
          <button className="btn" type="button" onClick={close}>Close</button>
        </div>
      </div>
    </ModalShell>
  );
}

export function ConfirmModal({ title, message, confirmLabel = "CONFIRM", danger = true, onConfirm, onCancel }) {
  const { closeRef, close } = useModalClose();
  const messageId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  return (
    <ModalShell
      title={title}
      onClose={onCancel}
      closeRef={closeRef}
      small
      role="alertdialog"
      titleColor={danger ? "danger" : "lime"}
      describedBy={messageId}
    >
      <div className="modal-body">
        <p id={messageId} className="text-copy text-muted">{message}</p>
        <div className="flex flex-col-reverse gap-2 pt-1 sm:flex-row sm:justify-end">
          <button className="btn" type="button" onClick={close} data-autofocus>Cancel</button>
          <button className={`btn ${danger ? "btn-danger" : "btn-primary"}`} type="button" onClick={onConfirm}>
            {confirmLabel}
          </button>
        </div>
      </div>
    </ModalShell>
  );
}

export function KeyModal({ token, onClose, notify }) {
  const { closeRef, close } = useModalClose();
  return (
    <ModalShell title="New access key" onClose={onClose} closeRef={closeRef}>
      <div className="modal-body">
        <p className="text-copy text-muted">
          The old key is dead and open control sockets were revoked. Save this key. It is shown once.
        </p>
        <div className="key-box">
          <code>{token}</code>
          <button
            className="icon-btn h-10 w-10 shrink-0 sm:h-8 sm:w-8"
            type="button"
            aria-label="Copy new access key"
            onClick={async () => {
              const ok = await copyText(token);
              notify(ok ? "info" : "error", ok ? "KEY COPIED" : "COPY FAILED", "Store it somewhere safe.");
            }}
          >
            <Icon name="i-copy" />
          </button>
        </div>
        <p className="flex items-start gap-2 text-copy text-faint">
          <Icon name="i-alert" className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" />
          <span>
            This deck reconnects automatically. Other browser tabs must paste the new key. If you installed the
            Token Bridge userscript, regenerate and reinstall it with this key.
          </span>
        </p>
        <div className="flex justify-end">
          <button className="btn btn-primary" type="button" onClick={close} data-autofocus>I saved it</button>
        </div>
      </div>
    </ModalShell>
  );
}
