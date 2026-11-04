import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Icon, ModalShell } from "./ModalShell.jsx";
import { fixedPrice, shortId } from "../lib/format.js";

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

function FieldError({ message }) {
  if (!message) return null;
  return <em className="field-error">{message}</em>;
}

function ToggleRow({ checked, onChange, title, hint, disabled = false }) {
  return (
    <label className="toggle-row">
      <span>
        {title}
        <small>{hint}</small>
      </span>
      <input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} hidden disabled={disabled} />
      <span className="switch" aria-hidden="true" />
    </label>
  );
}

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

export function TargetModal({ config, onClose, worker, notify }) {
  const editing = Boolean(config);
  const [itemId, setItemId] = useState(config?.itemId || "");
  const [itemType, setItemType] = useState(config?.itemType || "mutation_component");
  const [targetPrice, setTargetPrice] = useState(config ? String(config.targetPrice) : "");
  const [desiredQty, setDesiredQty] = useState(config ? String(config.desiredQty) : "");
  const [highStockThreshold, setHighStockThreshold] = useState(
    config ? String(config.highStockThreshold) : ""
  );
  const [minAcceptableQty, setMinAcceptableQty] = useState(
    config ? String(config.minAcceptableQty) : ""
  );
  const [allowPartialOrder, setAllowPartialOrder] = useState(config?.allowPartialOrder !== false);
  const [active, setActive] = useState(config?.active !== false);
  const [errors, setErrors] = useState({});
  const [busy, setBusy] = useState(false);
  const [auto, setAuto] = useState({
    state: config ? (config.label ? "ready" : "loading") : "idle",
    label: config?.label || (config ? "Resolving item name…" : "Waiting for an item ID…"),
    rarity: "",
    color: ""
  });
  const lookupSeq = useRef(0);
  const lookupRef = useRef(null);
  const editLookupDone = useRef(false);

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

  const maxSpend = useMemo(() => {
    const price = Number(targetPrice) || 0;
    const qty = Number(desiredQty) || 0;
    return price > 0 && qty > 0 ? `${fixedPrice(price * qty * 2)} RLT` : "—";
  }, [targetPrice, desiredQty]);

  const submit = async (event) => {
    event.preventDefault();
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
    if (Object.keys(localErrors).length > 0) return;

    setBusy(true);
    try {
      await worker.saveTarget(target, config?.itemId);
      notify("success", editing ? "TARGET UPDATED" : "TARGET DEPLOYED", auto.label || shortId(target.itemId));
      onClose();
    } catch (error) {
      if (error.fields && Object.keys(error.fields).length > 0) {
        setErrors(error.fields);
        notify("error", "CHECK THE FIELDS", "The worker rejected one or more values.");
      } else {
        notify("error", "SAVE FAILED", error.message);
      }
      setBusy(false);
    }
  };

  return (
    <ModalShell title={editing ? "EDIT TARGET" : "DEPLOY TARGET"} onClose={onClose}>
      <form className="modal-body" noValidate autoComplete="off" onSubmit={submit}>
        <div className="form-grid">
          <label className="field span-2">
            <span>ITEM ID / MARKETPLACE URL</span>
            <input
              className={`input${errors.itemId ? " is-invalid" : ""}`}
              name="itemId"
              value={itemId}
              onChange={(event) => setItemId(event.target.value)}
              placeholder="6571aa39ca8bfef34adcd9e9"
              readOnly={editing}
              spellCheck="false"
              required
            />
            <small>Paste the ID or the full Rollercoin marketplace URL. The label is resolved automatically.</small>
            <FieldError message={errors.itemId} />
          </label>

          <div className="auto-label span-2" data-auto-label data-state={auto.state}>
            <div className="auto-label-head">
              <span>AUTO LABEL</span>
              <span className="auto-label-side">
                {auto.rarity ? (
                  <span
                    className="auto-label-rarity"
                    style={auto.color ? { "--rarity": `#${auto.color}` } : undefined}
                  >
                    {auto.rarity}
                  </span>
                ) : null}
                {auto.state === "warn" ? (
                  <button className="auto-label-retry" type="button" onClick={() => lookupRef.current?.()}>
                    RETRY
                  </button>
                ) : null}
              </span>
            </div>
            <strong>{auto.label}</strong>
            <small>From Rollercoin item-info: rarity + item name. Falls back to the item name when rarity is absent.</small>
          </div>

          <label className="field">
            <span>TARGET PRICE (RLT)</span>
            <input
              className={`input${errors.targetPrice ? " is-invalid" : ""}`}
              name="targetPrice"
              value={targetPrice}
              onChange={(event) => setTargetPrice(event.target.value)}
              type="number"
              inputMode="decimal"
              step="0.000001"
              min="0.000001"
              placeholder="e.g. 0.100000"
              required
            />
            <small>Never pay more than this per item.</small>
            <FieldError message={errors.targetPrice} />
          </label>
          <label className="field">
            <span>DESIRED QTY</span>
            <input
              className={`input${errors.desiredQty ? " is-invalid" : ""}`}
              name="desiredQty"
              value={desiredQty}
              onChange={(event) => setDesiredQty(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              max="10000"
              placeholder="e.g. 10"
              required
            />
            <FieldError message={errors.desiredQty} />
          </label>
          <label className="field">
            <span>HIGH STOCK THRESHOLD</span>
            <input
              className={`input${errors.highStockThreshold ? " is-invalid" : ""}`}
              name="highStockThreshold"
              value={highStockThreshold}
              onChange={(event) => setHighStockThreshold(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              placeholder="e.g. 20"
              required
            />
            <small>At or above this stock, order qty doubles.</small>
            <FieldError message={errors.highStockThreshold} />
          </label>
          <label className="field">
            <span>MIN ACCEPTABLE QTY</span>
            <input
              className={`input${errors.minAcceptableQty ? " is-invalid" : ""}`}
              name="minAcceptableQty"
              value={minAcceptableQty}
              onChange={(event) => setMinAcceptableQty(event.target.value)}
              type="number"
              inputMode="numeric"
              step="1"
              min="1"
              placeholder="e.g. 1"
              required
            />
            <small>Partial fills below this are skipped.</small>
            <FieldError message={errors.minAcceptableQty} />
          </label>
          <div className="field">
            <span>MAX SPEND PER FILL</span>
            <output className="output-box">{maxSpend}</output>
            <small>Target price × doubled quantity ceiling.</small>
          </div>
        </div>

        <ToggleRow
          checked={allowPartialOrder}
          onChange={setAllowPartialOrder}
          title="ALLOW PARTIAL ORDER"
          hint="Buy whatever is available above the minimum."
        />
        <ToggleRow
          checked={active}
          onChange={setActive}
          title="ARM TARGET"
          hint="Start hunting as soon as it is saved."
        />

        <div className="modal-actions">
          <button className="btn" type="button" onClick={onClose}>
            CANCEL
          </button>
          <button className="btn btn-primary" type="submit" disabled={busy}>
            {busy ? (editing ? "SAVING…" : "DEPLOYING…") : editing ? "SAVE TARGET" : "DEPLOY TARGET"}
          </button>
        </div>
      </form>
    </ModalShell>
  );
}

export function SessionModal({ session, onClose, worker, notify }) {
  const [accessToken, setAccessToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [cookie, setCookie] = useState("");
  const [clearCookie, setClearCookie] = useState(false);
  const [errors, setErrors] = useState({});
  const [busy, setBusy] = useState(false);

  const submit = async (event) => {
    event.preventDefault();
    const body = {};
    if (accessToken.trim()) body.accessToken = accessToken.trim();
    if (refreshToken.trim()) body.refreshToken = refreshToken.trim();
    if (cookie.trim()) body.cookie = cookie.trim();
    if (clearCookie) body.clearCookie = true;
    if (!session.hasAccessToken && !body.accessToken && !clearCookie) {
      setErrors({ accessToken: "Paste an access token first." });
      return;
    }
    if (Object.keys(body).length === 0) {
      setErrors({ accessToken: "Nothing to update — paste a token or clear the form." });
      return;
    }
    setBusy(true);
    try {
      await worker.saveSession(body);
      notify("success", "SESSION SAVED", "The worker is re-linking to the market feed.");
      onClose();
    } catch (error) {
      if (error.fields && Object.keys(error.fields).length > 0) {
        setErrors(error.fields);
      } else {
        notify("error", "SESSION FAILED", error.message);
      }
      setBusy(false);
    }
  };

  return (
    <ModalShell title="LINK ROLLERCOIN SESSION" onClose={onClose}>
      <form className="modal-body" noValidate autoComplete="off" onSubmit={submit}>
        <p className="muted small">
          Grab the tokens from a logged-in rollercoin.com tab: DevTools → Application → Local
          Storage → <code>token</code> and <code>refreshToken</code>. The browser bridge can keep
          them in sync automatically.
        </p>
        <label className="field">
          <span>ACCESS TOKEN</span>
          <textarea
            className="input"
            rows="3"
            value={accessToken}
            onChange={(event) => setAccessToken(event.target.value)}
            spellCheck="false"
            placeholder={
              session.hasAccessToken
                ? `Current ${session.accessPreview || "token"} — leave blank to keep`
                : "Paste the Rollercoin access token"
            }
          />
          <FieldError message={errors.accessToken} />
        </label>
        <label className="field">
          <span>REFRESH TOKEN</span>
          <textarea
            className="input"
            rows="2"
            value={refreshToken}
            onChange={(event) => setRefreshToken(event.target.value)}
            spellCheck="false"
            placeholder={
              session.hasRefreshToken
                ? `Current ${session.refreshPreview || "token"} — leave blank to keep`
                : "Paste the refresh token"
            }
          />
          <FieldError message={errors.refreshToken} />
        </label>
        <label className="field">
          <span>CLOUDFLARE COOKIE <small className="inline-hint">OPTIONAL</small></span>
          <textarea
            className="input"
            rows="2"
            value={cookie}
            onChange={(event) => setCookie(event.target.value)}
            spellCheck="false"
            placeholder={
              session.hasCookie
                ? "A cookie is stored — leave blank to keep it"
                : "cf_clearance=…; __cf_bm=… (optional)"
            }
          />
          <small>Only needed if Rollercoin rejects API calls with a Cloudflare challenge.</small>
          <FieldError message={errors.cookie} />
        </label>
        {session.hasCookie ? (
          <ToggleRow
            checked={clearCookie}
            onChange={setClearCookie}
            title="CLEAR STORED COOKIE"
            hint="Remove the saved Cloudflare cookie without touching the tokens."
          />
        ) : null}
        <div className="modal-actions">
          <button className="btn" type="button" onClick={onClose}>
            CANCEL
          </button>
          <button className="btn btn-primary" type="submit" disabled={busy}>
            {busy ? "SAVING…" : "SAVE SESSION"}
          </button>
        </div>
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
    <ModalShell title="TOKEN BRIDGE" onClose={onClose} wide>
      <div className="modal-body">
        <div className="bridge-mode" role="tablist" aria-label="Bridge mode">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "console"}
            className={`bridge-mode-btn${mode === "console" ? " is-active" : ""}`}
            onClick={() => setMode("console")}
          >
            CONSOLE SNIPPET
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "userscript"}
            className={`bridge-mode-btn${mode === "userscript" ? " is-active" : ""}`}
            onClick={() => setMode("userscript")}
          >
            USERSCRIPT · AUTO
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

        <div className="code-wrap">
          <pre className="code-block"><code>{code || (error || "Loading bridge…")}</code></pre>
          <div className="bridge-actions">
            <button className="btn btn-primary btn-sm" type="button" onClick={copy}>
              <Icon name="i-copy" className="icon icon-xs" />
              {mode === "console" ? "COPY SNIPPET" : "COPY USERSCRIPT"}
            </button>
            {mode === "userscript" ? (
              <button className="btn btn-ghost btn-sm" type="button" onClick={download}>
                <Icon name="i-download" className="icon icon-xs" />
                DOWNLOAD .USER.JS
              </button>
            ) : null}
          </div>
        </div>

        {mode === "console" ? (
          <p className="hint">
            <Icon name="i-alert" className="icon icon-xs" />
            Console mode uses the page's <code>fetch</code>, so Rollercoin's CSP or browser privacy
            rules can block localhost with a CORS error. If that happens, switch to USERSCRIPT ·
            AUTO. It also must be pasted again after a reload.
          </p>
        ) : (
          <p className="hint">
            <Icon name="i-check" className="icon icon-xs" />
            Userscript mode uses <code>GM_xmlhttpRequest</code>, which bypasses page CORS/CSP.
            Install it once; after that you only need the worker running and a logged-in Rollercoin
            tab. Regenerate it if you rotate the access key.
          </p>
        )}
        <p className="hint">
          <Icon name="i-alert" className="icon icon-xs" />
          Both variants contain this deck's access key. Keep them private and never paste them into
          a public gist or chat.
        </p>
        <div className="modal-actions">
          <button className="btn" type="button" onClick={onClose}>CLOSE</button>
        </div>
      </div>
    </ModalShell>
  );
}

export function ConfirmModal({ title, message, confirmLabel = "CONFIRM", danger = true, onConfirm, onCancel }) {
  return (
    <ModalShell title={title} onClose={onCancel} small role="alertdialog">
      <div className="modal-body">
        <p className="confirm-message">{message}</p>
        <div className="modal-actions">
          <button className="btn" type="button" onClick={onCancel}>CANCEL</button>
          <button className={`btn ${danger ? "btn-danger" : "btn-primary"}`} type="button" onClick={onConfirm}>
            {confirmLabel}
          </button>
        </div>
      </div>
    </ModalShell>
  );
}

export function KeyModal({ token, onClose, notify }) {
  return (
    <ModalShell title="NEW ACCESS KEY" onClose={onClose}>
      <div className="modal-body">
        <p className="muted">
          The old key is dead and open control sockets were revoked. Save this key — it is shown once.
        </p>
        <div className="key-box">
          <code>{token}</code>
          <button
            className="icon-btn"
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
        <p className="hint">
          <Icon name="i-alert" className="icon icon-xs" />
          This deck reconnects automatically. Other browser tabs must paste the new key. If you
          installed the Token Bridge userscript, regenerate and reinstall it with this key.
        </p>
        <div className="modal-actions">
          <button className="btn btn-primary" type="button" onClick={onClose}>I SAVED IT</button>
        </div>
      </div>
    </ModalShell>
  );
}
