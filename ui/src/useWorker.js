import { useCallback, useEffect, useRef, useState } from "react";
import { WorkerClient, ApiError } from "./lib/client.js";
import { mergeHistory, fixedPrice, formatInt, formatSignedRlt } from "./lib/format.js";
import { sound } from "./lib/sound.js";
import {
  DEFAULT_SETTINGS,
  LOOT_LOG_PAGE_SIZE,
  LIMITS,
  MAX_REPORT_PRODUCTS
} from "./lib/constants.js";

const URL_KEY = "turuc.url";
const TOKEN_KEY = "turuc.key";
/**
 * Marks the stored key as persistent. Without it the key only lives in
 * sessionStorage, so closing the tab drops the session and asks for login
 * again; with it the key survives a browser restart but every visitor of
 * this browser profile can reuse it.
 */
const REMEMBER_KEY = "turuc.remember";

/**
 * Reads the access key from wherever it was last stored. The persistent copy
 * wins so that toggling "remember" off still picks up a key saved earlier
 * during the same tab.
 */
function readStoredToken() {
  try {
    return localStorage.getItem(TOKEN_KEY) || sessionStorage.getItem(TOKEN_KEY) || "";
  } catch {
    return "";
  }
}

function isRemembered() {
  try {
    return localStorage.getItem(REMEMBER_KEY) === "on";
  } catch {
    return false;
  }
}

/**
 * Persists the access key. `remember` moves it to localStorage so a closed tab
 * no longer forces a login; otherwise it stays scoped to this tab only.
 * The opposite store is always cleared so exactly one copy exists.
 */
function writeStoredToken(token, remember) {
  try {
    if (remember) {
      localStorage.setItem(TOKEN_KEY, token);
      localStorage.setItem(REMEMBER_KEY, "on");
      sessionStorage.removeItem(TOKEN_KEY);
    } else {
      sessionStorage.setItem(TOKEN_KEY, token);
      localStorage.removeItem(TOKEN_KEY);
      localStorage.removeItem(REMEMBER_KEY);
    }
  } catch {
    // Storage can be unavailable (private mode, quota); the in-memory session
    // still works, it just will not be restored.
  }
}

function clearStoredToken() {
  try {
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(REMEMBER_KEY);
    sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    // Nothing to clean up.
  }
}

/**
 * Newest loot entries kept in browser state for the live panel. The full log
 * lives in SQLite and is paged through /api/history, so this window only has
 * to bridge what arrives over the socket.
 */
const LOOT_LIVE_WINDOW = 80;

function upsertLootHistory(previous, entry) {
  if (!previous) return previous;
  const history = Array.isArray(previous.history) ? previous.history : [];
  const index = history.findIndex((item) => item.id === entry.id);
  if (index >= 0) {
    const updated = [...history];
    updated[index] = { ...updated[index], ...entry };
    return { ...previous, history: updated };
  }
  return {
    ...previous,
    history: [entry, ...history].slice(0, LOOT_LIVE_WINDOW),
    historyTotal: (Number(previous.historyTotal) || 0) + 1
  };
}

function messageForError(error) {
  if (error?.status === 401) return "ACCESS DENIED — this key was rejected. Check the worker terminal.";
  if (error?.status === 403) return "ACCESS DENIED — forbidden.";
  if (error?.status === 429) return "TOO MANY ATTEMPTS — wait a minute, then try again.";
  if (error?.status === 0) return error.message;
  return error?.message || "Something went wrong while contacting the worker.";
}

function nextMutationVersion(versions, key) {
  const version = (versions.current.get(key) || 0) + 1;
  versions.current.set(key, version);
  return version;
}

function invalidateMutationVersions(versions) {
  for (const [key, version] of versions.current) {
    versions.current.set(key, version + 1);
  }
}

function normalizeTargetId(itemId) {
  const value = String(itemId || "").trim();
  if (!/^https?:\/\//i.test(value)) return value;
  try {
    return new URL(value).pathname.split("/").filter(Boolean).at(-1) || value;
  } catch {
    return value;
  }
}

function previewToken(token) {
  const value = String(token || "");
  if (!value) return null;
  return value.length <= 12
    ? `${value.slice(0, 2)}••••${value.slice(-2)}`
    : `${value.slice(0, 5)}••••${value.slice(-4)}`;
}

function optimisticSessionPatch(previous = {}, patch = {}) {
  const next = {
    ...previous,
    source: "ui",
    updatedAt: Date.now(),
    lastError: null
  };
  if (patch.clearAccessToken) {
    next.hasAccessToken = false;
    next.accessPreview = null;
    next.expiresAt = null;
  } else if (typeof patch.accessToken === "string") {
    const token = patch.accessToken.trim().replace(/^Bearer\s+/i, "");
    next.hasAccessToken = Boolean(token);
    next.accessPreview = previewToken(token);
    next.expiresAt = null;
  }
  if (patch.clearRefreshToken) {
    next.hasRefreshToken = false;
    next.refreshPreview = null;
  } else if (typeof patch.refreshToken === "string") {
    const token = patch.refreshToken.trim();
    next.hasRefreshToken = Boolean(token);
    next.refreshPreview = previewToken(token);
  }
  if (patch.clearCookie) next.hasCookie = false;
  else if (typeof patch.cookie === "string") next.hasCookie = Boolean(patch.cookie.trim());
  next.refreshing = false;
  return next;
}

function deriveBackupStats(data, now) {
  const orders = Array.isArray(data.orders) ? data.orders : [];
  const filled = orders.filter((order) => order?.status !== "failed");
  const derived = {
    since: Number(data.createdAt) || now,
    profitRlt: filled.reduce((sum, order) => sum + (Number(order?.profitRlt) || 0), 0),
    spentRlt: filled.reduce((sum, order) => sum + (Number(order?.totalPrice) || 0), 0),
    bought: filled.reduce((sum, order) => sum + (Number(order?.qty) || 0), 0),
    filled: filled.length,
    failed: orders.length - filled.length,
    ordersSent: orders.length
  };
  return data.stats && typeof data.stats === "object" && !Array.isArray(data.stats)
    ? { ...derived, ...data.stats }
    : derived;
}

function deriveBackupReport(data, since) {
  let products = Array.isArray(data.reportProducts)
    ? data.reportProducts.filter((product) => product && typeof product === "object")
    : [];
  if (products.length === 0 && Array.isArray(data.orders)) {
    const grouped = new Map();
    for (const order of data.orders) {
      const key = String(order?.itemId || order?.label || "unknown").toLowerCase();
      const product = grouped.get(key) || {
        productKey: key,
        itemId: order?.itemId || "",
        label: order?.label || "",
        fills: 0,
        failed: 0,
        qty: 0,
        spentRlt: 0,
        profitRlt: 0,
        firstAt: Number(order?.at) || Date.now(),
        lastAt: Number(order?.at) || Date.now()
      };
      if (order?.status === "failed") product.failed += 1;
      else {
        product.fills += 1;
        product.qty += Number(order?.qty) || 0;
        product.spentRlt += Number(order?.totalPrice) || 0;
        product.profitRlt += Number(order?.profitRlt) || 0;
      }
      product.firstAt = Math.min(product.firstAt, Number(order?.at) || product.firstAt);
      product.lastAt = Math.max(product.lastAt, Number(order?.at) || product.lastAt);
      grouped.set(key, product);
    }
    products = [...grouped.values()];
  }
  products = [...products]
    .sort((a, b) => (Number(b.profitRlt) || 0) - (Number(a.profitRlt) || 0))
    .slice(0, MAX_REPORT_PRODUCTS);

  const items = products.map((product) => {
    const qty = Number(product.qty) || 0;
    const spentRlt = Number(product.spentRlt) || 0;
    const profitRlt = Number(product.profitRlt) || 0;
    const fills = Number(product.fills) || 0;
    const failed = Number(product.failed) || 0;
    const completed = fills + failed;
    return {
      ...product,
      avgBuy: qty > 0 ? spentRlt / qty : 0,
      marginPct: spentRlt > 0 ? Math.round((profitRlt / spentRlt) * 1000) / 10 : null,
      successPct: completed > 0 ? Math.round((fills / completed) * 100) : null
    };
  }).sort((a, b) => b.profitRlt - a.profitRlt || b.spentRlt - a.spentRlt);
  const totals = items.reduce((sum, item) => {
    sum.fills += Number(item.fills) || 0;
    sum.failed += Number(item.failed) || 0;
    sum.qty += Number(item.qty) || 0;
    sum.spentRlt += Number(item.spentRlt) || 0;
    sum.profitRlt += Number(item.profitRlt) || 0;
    return sum;
  }, { products: items.length, fills: 0, failed: 0, qty: 0, spentRlt: 0, profitRlt: 0, marginPct: null });
  totals.products = items.length;
  totals.marginPct = totals.spentRlt > 0
    ? Math.round((totals.profitRlt / totals.spentRlt) * 1000) / 10
    : null;
  return { since, totals, items };
}

function optimisticBackupSnapshot(previous, backup, restoreCredentials) {
  const data = backup?.data || {};
  const now = Date.now();
  const stats = deriveBackupStats(data, now);
  const orders = Array.isArray(data.orders) ? data.orders : [];
  const rawSession = restoreCredentials ? backup?.credentials?.session : null;
  const session = rawSession && (rawSession.accessToken || rawSession.refreshToken || rawSession.cookie)
    ? optimisticSessionPatch(previous.session, {
        accessToken: rawSession.accessToken || "",
        refreshToken: rawSession.refreshToken || "",
        cookie: rawSession.cookie || ""
      })
    : previous.session;
  const settings = {
    ...DEFAULT_SETTINGS,
    ...(data.settings && typeof data.settings === "object" && !Array.isArray(data.settings)
      ? data.settings
      : {})
  };
  const reportSince = Number(data.reportSince) || Number(stats.since) || now;
  return {
    ...previous,
    settings,
    configs: (Array.isArray(data.targets) ? data.targets.slice(0, LIMITS.configsMax) : [])
      .filter((config) => config && typeof config.itemId === "string")
      .map((config) => ({ ...config, runtime: null })),
    stats,
    report: deriveBackupReport(data, reportSince),
    history: orders.slice(-LOOT_LOG_PAGE_SIZE).reverse(),
    historyTotal: orders.length,
    session: session ? { ...session, autoRefresh: settings.autoRefresh } : session
  };
}

function emptyLatencySnapshot(previous, measuredAt) {
  if (!previous) return previous;
  const empty = (channel) => ({
    ...channel,
    count: 0,
    last: null,
    lastAt: null,
    lastOk: null,
    min: null,
    max: null,
    avg: null,
    p50: null,
    p95: null,
    jitter: null,
    errors: 0,
    lastError: null,
    recent: []
  });
  return {
    ...previous,
    channels: Object.fromEntries(
      Object.entries(previous.channels || {}).map(([key, value]) => [key, empty(value)])
    ),
    freshest: null,
    measuredAt
  };
}

export function useWorker({ notify }) {
  const clientRef = useRef(null);
  const logIdsRef = useRef(new Set());
  const configMutationVersions = useRef(new Map());
  const settingsMutationVersions = useRef(new Map());
  const pendingSettingsMutations = useRef(new Map());
  const sessionMutationVersions = useRef(new Map());
  const statsMutationVersions = useRef(new Map());
  const backupMutationVersions = useRef(new Map());
  const gatewayMutationVersions = useRef(new Map());
  const notifyRef = useRef(notify);
  const gatewayRef = useRef(null);
  const chattyRef = useRef(false);

  const [snapshot, setSnapshot] = useState(null);
  const [logs, setLogs] = useState([]);
  const [status, setStatus] = useState("idle");
  const [latency, setLatency] = useState(null);
  const [authError, setAuthError] = useState("");
  const [busy, setBusy] = useState(false);
  const [connected, setConnected] = useState(false);
  const [historyRevision, setHistoryRevision] = useState(0);

  notifyRef.current = notify;

  const appendLog = useCallback((entry) => {
    if (!entry || typeof entry.message !== "string") return;
    const id = String(entry.id ?? `${entry.at}-${entry.message}`);
    if (logIdsRef.current.has(id)) return;
    logIdsRef.current.add(id);
    setLogs((previous) => {
      const next = [...previous, entry];
      return next.length > 400 ? next.slice(-400) : next;
    });
  }, []);

  const appendLogs = useCallback(
    (entries) => {
      if (!Array.isArray(entries)) return;
      for (const entry of entries) appendLog(entry);
    },
    [appendLog]
  );

  const clearLogs = useCallback(() => {
    logIdsRef.current.clear();
    setLogs([]);
  }, []);

  const applySnapshot = useCallback((data) => {
    if (!data) return;
    nextMutationVersion(backupMutationVersions, "restore");
    nextMutationVersion(statsMutationVersions, "reset");
    nextMutationVersion(sessionMutationVersions, "session");
    nextMutationVersion(gatewayMutationVersions, "reconnect");
    invalidateMutationVersions(configMutationVersions);
    for (const config of data.configs || []) {
      if (config?.itemId) {
        nextMutationVersion(configMutationVersions, String(config.itemId).toLowerCase());
      }
    }
    const next = { ...data };
    if (pendingSettingsMutations.current.size > 0) {
      next.settings = { ...(data.settings || {}) };
      for (const [key, mutation] of pendingSettingsMutations.current) {
        next.settings[key] = mutation.value;
      }
    }
    setSnapshot(next);
    if (data.gateway) gatewayRef.current = data.gateway.status;
    setConnected(true);
  }, []);

  const updateConfig = useCallback((itemId, updater) => {
    setSnapshot((previous) => {
      if (!previous || !Array.isArray(previous.configs)) return previous;
      const key = String(itemId || "").toLowerCase();
      let changed = false;
      const configs = previous.configs.map((config) => {
        if (config.itemId.toLowerCase() !== key) return config;
        changed = true;
        return { ...config, runtime: updater(config.runtime || {}, config) };
      });
      return changed ? { ...previous, configs } : previous;
    });
  }, []);

  const handleEvent = useCallback(
    (type, data) => {
      switch (type) {
        case "log":
          appendLog(data);
          break;
        case "market": {
          const bucket =
            Number(data.bucket) || Math.floor((data.at || Date.now()) / 60_000) * 60_000;
          // `price` is the raw best ask for the readout; `close` is the
          // worker's duration-weighted minute median, which is what the chart
          // plots so a transient bait price cannot bend the line. `low`/`high`
          // widen the intra-minute band the expanded chart draws behind it.
          const close = Number(data.close);
          const low = Number(data.low);
          const high = Number(data.high);
          const plotted = Number.isFinite(close) ? close : data.price;
          updateConfig(data.itemId, (runtime) => ({
            ...runtime,
            itemId: data.itemId,
            label: data.label,
            lastPrice: data.price,
            lastStock: data.stock,
            lastSeenAt: data.at,
            itemType: data.itemType,
            allowedPriceMin: Number.isFinite(data.allowedPriceMin)
              ? data.allowedPriceMin
              : runtime.allowedPriceMin,
            allowedPriceMax: Number.isFinite(data.allowedPriceMax)
              ? data.allowedPriceMax
              : runtime.allowedPriceMax,
            allowedAt: data.allowedAt ?? runtime.allowedAt,
            trend: data.trend ?? runtime.trend,
            history: mergeHistory(runtime.history, plotted, bucket, low, high)
          }));
          break;
        }
        case "price": {
          // A sampled close: the worker saw no frame for this item this minute,
          // so the price did not move. The chart stays flat without pretending
          // the item was just seen (`lastSeenAt` is deliberately untouched).
          const bucket =
            Number(data.bucket) || Math.floor((data.at || Date.now()) / 60_000) * 60_000;
          const price = Number(data.price);
          if (!Number.isFinite(price)) break;
          updateConfig(data.itemId, (runtime) => ({
            ...runtime,
            itemId: data.itemId,
            history: mergeHistory(
              runtime.history,
              price,
              bucket,
              Number(data.low),
              Number(data.high)
            )
          }));
          break;
        }
        case "order": {
          nextMutationVersion(statsMutationVersions, "reset");
          const works = (runtime) => {
            const next = { ...runtime };
            if (data.phase === "submitted") {
              next.inflight = true;
              next.lastOrderAt = data.at;
              next.lastResult = {
                status: "submitted",
                at: data.at || Date.now(),
                qty: data.qty,
                price: data.unitPrice
              };
            } else if (data.phase === "accepted") {
              next.inflight = false;
              next.lastResult = {
                status: "accepted",
                at: data.at || Date.now(),
                qty: data.qty,
                orderId: data.orderId
              };
            } else if (data.phase === "dry") {
              next.inflight = false;
              next.lastResult = {
                status: "dry",
                at: data.at || Date.now(),
                qty: data.qty,
                price: data.unitPrice
              };
            } else if (data.phase === "failed") {
              next.inflight = false;
              next.failures = (Number(next.failures) || 0) + 1;
              next.lastResult = {
                status: "rejected",
                at: data.at || Date.now(),
                reason: data.reason
              };
            } else if (data.phase === "filled") {
              next.inflight = false;
              next.fills = (Number(next.fills) || 0) + 1;
              if (Number.isFinite(data.stockAfter)) next.lastStock = data.stockAfter;
              next.lastResult = {
                status: "filled",
                at: data.at || Date.now(),
                qty: data.qty,
                price: data.unitPrice,
                profit: data.profitRlt
              };
            }
            return next;
          };
          updateConfig(data.itemId, works);

          if (data.phase === "filled") {
            const entry = { ...data };
            setSnapshot((previous) => upsertLootHistory(previous, entry));
            notifyRef.current?.(
              "success",
              "LOOT SECURED",
              `${data.label || "ITEM"} ×${formatInt(data.qty)} · ${formatSignedRlt(data.profitRlt)} RLT`
            );
            sound.play("fill");
          } else if (data.phase === "failed") {
            notifyRef.current?.(
              "error",
              "ORDER MISSED",
              `${data.label || "ITEM"} — ${data.reason || "no items obtained"}`
            );
            sound.play("error");
          } else if (data.phase === "dry") {
            notifyRef.current?.(
              "info",
              "SHADOW SIGNAL",
              `${data.label || "ITEM"} ×${formatInt(data.qty)} @ ${fixedPrice(data.unitPrice)} RLT`
            );
          }
          break;
        }
        case "history": {
          nextMutationVersion(statsMutationVersions, "reset");
          if (!data?.id) break;
          setSnapshot((previous) => upsertLootHistory(previous, data));
          break;
        }
        case "stats":
          nextMutationVersion(statsMutationVersions, "reset");
          setSnapshot((previous) => (previous ? { ...previous, stats: data } : previous));
          break;
        case "report":
          nextMutationVersion(statsMutationVersions, "reset");
          setSnapshot((previous) => (previous ? { ...previous, report: data } : previous));
          break;
        case "session":
          nextMutationVersion(sessionMutationVersions, "session");
          setSnapshot((previous) => (previous ? { ...previous, session: data } : previous));
          break;
        case "gateway": {
          nextMutationVersion(gatewayMutationVersions, "reconnect");
          const previousStatus = gatewayRef.current;
          gatewayRef.current = data?.status || gatewayRef.current;
          setSnapshot((previous) => (previous ? { ...previous, gateway: data } : previous));
          if (previousStatus && previousStatus !== "online" && data?.status === "online" && chattyRef.current) {
            notifyRef.current?.("success", "MARKET FEED ONLINE", "Streaming marketplace frames.");
            sound.play("connect");
          } else if (previousStatus === "online" && data?.status === "offline") {
            notifyRef.current?.("warn", "FEED OFFLINE", "The worker is reconnecting to Rollercoin.");
          } else if (previousStatus && previousStatus !== "auth-error" && data?.status === "auth-error") {
            notifyRef.current?.("error", "FEED REJECTED", "Rollercoin refused the session. Paste a fresh token.");
          }
          break;
        }
        case "latency":
          setSnapshot((previous) => (previous ? { ...previous, latency: data } : previous));
          break;
        case "benchmark":
          if (data) {
            setSnapshot((previous) => (previous ? { ...previous, benchmark: data } : previous));
          }
          break;
        case "system":
          if (data) {
            setSnapshot((previous) => (previous ? { ...previous, system: data } : previous));
          }
          break;
        case "settings":
          setSnapshot((previous) => {
            if (!previous) return previous;
            const settings = { ...(data || {}) };
            for (const [key, mutation] of pendingSettingsMutations.current) {
              if (Object.prototype.hasOwnProperty.call(settings, key)) {
                settings[key] = mutation.value;
              }
            }
            return { ...previous, settings };
          });
          break;
        case "configs":
          nextMutationVersion(statsMutationVersions, "reset");
          if (Array.isArray(data)) {
            invalidateMutationVersions(configMutationVersions);
            for (const config of data) {
              if (config?.itemId) {
                nextMutationVersion(configMutationVersions, String(config.itemId).toLowerCase());
              }
            }
            setSnapshot((previous) => (previous ? { ...previous, configs: data } : previous));
          }
          break;
        case "history-cleared":
          nextMutationVersion(statsMutationVersions, "reset");
          nextMutationVersion(backupMutationVersions, "restore");
          setSnapshot((previous) =>
            previous
              ? {
                  ...previous,
                  history: [],
                  historyTotal: 0,
                  report: { since: Date.now(), items: [], totals: null }
                }
              : previous
          );
          break;
        default:
          break;
      }
    },
    [appendLog, updateConfig]
  );

  useEffect(() => {
    const client = new WorkerClient({
      onSnapshot: applySnapshot,
      onEvent: handleEvent,
      onStatus: setStatus,
      onLatency: setLatency,
      onLogs: appendLogs,
      onAuthFailure: (message) => {
        setAuthError(message);
        setSnapshot(null);
        setConnected(false);
        clearStoredToken();
        clientRef.current?.logout();
      }
    });
    clientRef.current = client;

    const savedKey = readStoredToken();
    const savedUrl = localStorage.getItem(URL_KEY);
    if (savedKey) {
      client
        .login(savedUrl || window.location.origin, savedKey)
        .then((state) => {
          applySnapshot(state);
          chattyRef.current = true;
          sound.play("connect");
        })
        .catch((error) => {
          setAuthError(messageForError(error));
          clearStoredToken();
        });
    }

    return () => client.dispose();
  }, [applySnapshot, appendLogs, handleEvent]);

  const login = useCallback(
    async (url, token, remember = true) => {
      setBusy(true);
      setAuthError("");
      try {
        const state = await clientRef.current.login(url, token);
        writeStoredToken(token, remember);
        localStorage.setItem(URL_KEY, state ? clientRef.current.baseUrl : url);
        applySnapshot(state);
        setStatus("online");
        chattyRef.current = true;
        sound.play("connect");
        return true;
      } catch (error) {
        setAuthError(messageForError(error));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [applySnapshot]
  );

  const logout = useCallback(() => {
    clientRef.current?.logout();
    invalidateMutationVersions(configMutationVersions);
    invalidateMutationVersions(settingsMutationVersions);
    invalidateMutationVersions(sessionMutationVersions);
    invalidateMutationVersions(statsMutationVersions);
    invalidateMutationVersions(backupMutationVersions);
    invalidateMutationVersions(gatewayMutationVersions);
    pendingSettingsMutations.current.clear();
    clearStoredToken();
    setSnapshot(null);
    setConnected(false);
    setAuthError("");
    setStatus("idle");
    setLatency(null);
    clearLogs();
    chattyRef.current = false;
    gatewayRef.current = null;
  }, [clearLogs]);

  const replaceAccessKey = useCallback((token) => {
    // A rotated key inherits whatever storage mode the session already uses,
    // so a remembered deck stays remembered and a tab-scoped one stays scoped.
    writeStoredToken(token, isRemembered());
    clientRef.current?.replaceToken(token);
  }, []);

  const request = useCallback((path, options) => clientRef.current.request(path, options), []);
  const requestText = useCallback((path, options) => clientRef.current.requestText(path, options), []);
  const requestBlob = useCallback((path, options) => clientRef.current.requestBlob(path, options), []);
  const resync = useCallback(() => clientRef.current?.resync(), []);
  const getConnection = useCallback(
    () => ({
      baseUrl: clientRef.current?.baseUrl || window.location.origin,
      token: clientRef.current?.token || ""
    }),
    []
  );

  const updateConfigSnapshot = useCallback((itemId, config, insertAt = null) => {
    const key = String(itemId || "").toLowerCase();
    setSnapshot((previous) => {
      if (!previous || !Array.isArray(previous.configs)) return previous;
      const index = previous.configs.findIndex(
        (entry) => entry.itemId.toLowerCase() === key
      );
      const configs = [...previous.configs];
      if (config === null) {
        if (index < 0) return previous;
        configs.splice(index, 1);
      } else if (index >= 0) {
        configs[index] = config;
      } else {
        const position = Number.isInteger(insertAt)
          ? Math.max(0, Math.min(insertAt, configs.length))
          : configs.length;
        configs.splice(position, 0, config);
      }
      return { ...previous, configs };
    });
  }, []);

  const updateSettingsOptimistically = useCallback(
    async (patch, path, options) => {
      const keys = Object.keys(patch);
      const previousSettings = { ...(snapshot?.settings || {}) };
      const versions = new Map(
        keys.map((key) => [key, nextMutationVersion(settingsMutationVersions, key)])
      );
      for (const key of keys) {
        pendingSettingsMutations.current.set(key, { version: versions.get(key), value: patch[key] });
      }

      setSnapshot((previous) =>
        previous
          ? { ...previous, settings: { ...(previous.settings || {}), ...patch } }
          : previous
      );

      try {
        const result = await request(path, options);
        if (result?.settings) {
          setSnapshot((previous) => {
            if (!previous) return previous;
            const settings = { ...(previous.settings || {}) };
            for (const key of keys) {
              if (settingsMutationVersions.current.get(key) !== versions.get(key)) continue;
              pendingSettingsMutations.current.delete(key);
              settings[key] = result.settings[key];
            }
            return { ...previous, settings };
          });
        }
        return result;
      } catch (error) {
        setSnapshot((previous) => {
          if (!previous) return previous;
          const settings = { ...(previous.settings || {}) };
          for (const key of keys) {
            if (settingsMutationVersions.current.get(key) !== versions.get(key)) continue;
            pendingSettingsMutations.current.delete(key);
            if (Object.prototype.hasOwnProperty.call(previousSettings, key)) {
              settings[key] = previousSettings[key];
            } else {
              delete settings[key];
            }
          }
          return { ...previous, settings };
        });
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot]
  );

  const saveTarget = useCallback(
    async (target, originalId) => {
      const itemId = originalId || normalizeTargetId(target.itemId);
      const key = String(itemId).toLowerCase();
      const version = nextMutationVersion(configMutationVersions, key);
      const previousConfig = snapshot?.configs?.find(
        (config) => config.itemId.toLowerCase() === key
      ) || null;
      const optimisticConfig = {
        ...(previousConfig || {}),
        ...target,
        itemId,
        runtime: previousConfig?.runtime ?? null
      };
      updateConfigSnapshot(itemId, optimisticConfig);

      const path = originalId
        ? `/api/configs/${encodeURIComponent(originalId)}`
        : "/api/configs";
      try {
        const result = await request(path, { method: originalId ? "PATCH" : "POST", body: target });
        const confirmed = result.configs?.find(
          (config) => config.itemId.toLowerCase() === key
        );
        if (configMutationVersions.current.get(key) === version && confirmed) {
          updateConfigSnapshot(itemId, confirmed);
        }
        setTimeout(() => clientRef.current?.resync(), 1500);
        return result;
      } catch (error) {
        if (configMutationVersions.current.get(key) === version) {
          updateConfigSnapshot(itemId, previousConfig);
        }
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot, updateConfigSnapshot]
  );

  const deleteTarget = useCallback(
    async (itemId) => {
      const key = String(itemId).toLowerCase();
      const version = nextMutationVersion(configMutationVersions, key);
      const previousConfig = snapshot?.configs?.find(
        (config) => config.itemId.toLowerCase() === key
      ) || null;
      const previousIndex = previousConfig
        ? snapshot.configs.findIndex((config) => config.itemId.toLowerCase() === key)
        : null;
      updateConfigSnapshot(itemId, null);
      try {
        const result = await request(`/api/configs/${encodeURIComponent(itemId)}`, {
          method: "DELETE"
        });
        if (configMutationVersions.current.get(key) === version) {
          updateConfigSnapshot(itemId, null);
        }
        return result;
      } catch (error) {
        if (configMutationVersions.current.get(key) === version) {
          updateConfigSnapshot(itemId, previousConfig, previousIndex);
        }
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot, updateConfigSnapshot]
  );

  const toggleTarget = useCallback(
    async (itemId, active) => {
      const key = String(itemId).toLowerCase();
      const version = nextMutationVersion(configMutationVersions, key);
      const previousConfig = snapshot?.configs?.find(
        (config) => config.itemId.toLowerCase() === key
      ) || null;
      if (previousConfig) updateConfigSnapshot(itemId, { ...previousConfig, active });
      try {
        const result = await request(`/api/configs/${encodeURIComponent(itemId)}`, {
          method: "PATCH",
          body: { active }
        });
        const confirmed = result.configs?.find(
          (config) => config.itemId.toLowerCase() === key
        );
        if (configMutationVersions.current.get(key) === version && confirmed) {
          updateConfigSnapshot(itemId, confirmed);
        }
        return result;
      } catch (error) {
        if (configMutationVersions.current.get(key) === version && previousConfig) {
          updateConfigSnapshot(itemId, previousConfig);
        }
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot, updateConfigSnapshot]
  );

  const saveSettings = useCallback(
    (patch) =>
      updateSettingsOptimistically(patch, "/api/settings", {
        method: "PATCH",
        body: patch
      }),
    [updateSettingsOptimistically]
  );

  const refreshSession = useCallback(async () => {
    const version = nextMutationVersion(sessionMutationVersions, "session");
    const previousSession = snapshot?.session || null;
    const attemptedAt = Date.now();
    setSnapshot((previous) => previous
      ? {
          ...previous,
          session: {
            ...(previous.session || {}),
            refreshing: true,
            lastAttemptAt: attemptedAt,
            lastFailureAt: null
          }
        }
      : previous);
    try {
      const result = await request("/api/session/refresh", { method: "POST" });
      if (sessionMutationVersions.current.get("session") === version && result?.session) {
        setSnapshot((previous) => previous ? { ...previous, session: result.session } : previous);
      }
      return result;
    } catch (error) {
      if (sessionMutationVersions.current.get("session") === version) {
        setSnapshot((previous) => previous ? { ...previous, session: previousSession } : previous);
      }
      void clientRef.current?.resync();
      throw error;
    }
  }, [request, snapshot]);

  const saveSession = useCallback(
    async (body) => {
      const version = nextMutationVersion(sessionMutationVersions, "session");
      const previousSession = snapshot?.session || null;
      setSnapshot((previous) => previous
        ? { ...previous, session: optimisticSessionPatch(previous.session, body) }
        : previous);
      try {
        const result = await request("/api/session", { method: "POST", body });
        if (sessionMutationVersions.current.get("session") === version && result?.session) {
          setSnapshot((previous) => previous ? { ...previous, session: result.session } : previous);
        }
        return result;
      } catch (error) {
        if (sessionMutationVersions.current.get("session") === version) {
          setSnapshot((previous) => previous ? { ...previous, session: previousSession } : previous);
        }
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot]
  );

  const clearSession = useCallback(async () => {
    const version = nextMutationVersion(sessionMutationVersions, "session");
    const previousSession = snapshot?.session || null;
    const cleared = optimisticSessionPatch(previousSession, {
      clearAccessToken: true,
      clearRefreshToken: true,
      clearCookie: true
    });
    setSnapshot((previous) => previous ? { ...previous, session: cleared } : previous);
    try {
      const result = await request("/api/session", { method: "DELETE" });
      if (sessionMutationVersions.current.get("session") === version && result?.session) {
        setSnapshot((previous) => previous ? { ...previous, session: result.session } : previous);
      }
      return result;
    } catch (error) {
      if (sessionMutationVersions.current.get("session") === version) {
        setSnapshot((previous) => previous ? { ...previous, session: previousSession } : previous);
      }
      void clientRef.current?.resync();
      throw error;
    }
  }, [request, snapshot]);

  const rotateAccessKey = useCallback(
    async () => {
      const result = await request("/api/auth/rotate", { method: "POST" });
      replaceAccessKey(result.token);
      setSnapshot((previous) =>
        previous
          ? {
              ...previous,
              workerTokenPreview: result.preview,
              accessKeyPinned: false
            }
          : previous
      );
      return result;
    },
    [replaceAccessKey, request]
  );

  const resetStats = useCallback(async () => {
    const version = nextMutationVersion(statsMutationVersions, "reset");
    setHistoryRevision((revision) => revision + 1);
    const resetAt = Date.now();
    const prior = {
      stats: snapshot?.stats,
      report: snapshot?.report,
      history: snapshot?.history,
      historyTotal: snapshot?.historyTotal,
      latency: snapshot?.latency,
      configs: snapshot?.configs
    };
    setSnapshot((previous) => previous
      ? {
          ...previous,
          stats: {
            since: resetAt,
            profitRlt: 0,
            spentRlt: 0,
            bought: 0,
            filled: 0,
            failed: 0,
            ordersSent: 0
          },
          report: {
            since: resetAt,
            items: [],
            totals: { products: 0, fills: 0, failed: 0, qty: 0, spentRlt: 0, profitRlt: 0, marginPct: null }
          },
          history: [],
          historyTotal: 0,
          latency: emptyLatencySnapshot(previous.latency, resetAt),
          configs: previous.configs.map((config) => ({
            ...config,
            runtime: config.runtime
              ? { ...config.runtime, fills: 0, failures: 0, lastResult: null }
              : config.runtime
          }))
        }
      : previous);
    try {
      const result = await request("/api/history", { method: "DELETE" });
      if (statsMutationVersions.current.get("reset") === version && result?.stats) {
        setSnapshot((previous) => previous ? { ...previous, stats: result.stats } : previous);
      }
      return result;
    } catch (error) {
      if (statsMutationVersions.current.get("reset") === version) {
        setSnapshot((previous) => previous
          ? {
              ...previous,
              stats: prior.stats,
              report: prior.report,
              history: prior.history,
              historyTotal: prior.historyTotal,
              latency: prior.latency,
              configs: prior.configs
            }
          : previous);
      }
      setHistoryRevision((revision) => revision + 1);
      void clientRef.current?.resync();
      throw error;
    }
  }, [request, snapshot]);

  /** Downloads the portable JSON backup exactly as the worker wrote it. */
  const exportBackup = useCallback(
    ({ credentials = false } = {}) => {
      const query = credentials ? "?credentials=1" : "";
      return requestText(`/api/backup${query}`, { timeout: 120_000 });
    },
    [requestText]
  );

  /** Downloads a consistent `.db` copy for offline disaster recovery. */
  const exportDatabase = useCallback(
    () => requestBlob("/api/backup?format=sqlite", { timeout: 120_000 }),
    [requestBlob]
  );

  const importBackup = useCallback(
    async (backup, { restoreCredentials = true } = {}) => {
      const version = nextMutationVersion(backupMutationVersions, "restore");
      const previous = snapshot;
      setHistoryRevision((revision) => revision + 1);
      setSnapshot((current) => current
        ? optimisticBackupSnapshot(current, backup, restoreCredentials)
        : current);
      try {
        const result = await request(`/api/backup?credentials=${restoreCredentials ? 1 : 0}`, {
          method: "POST",
          body: backup,
          timeout: 300_000
        });
        // Reconcile the complete imported state even if a socket frame was missed.
        await clientRef.current?.resync();
        setHistoryRevision((revision) => revision + 1);
        return result;
      } catch (error) {
        if (backupMutationVersions.current.get("restore") === version) {
          setSnapshot(previous);
        }
        setHistoryRevision((revision) => revision + 1);
        void clientRef.current?.resync();
        throw error;
      }
    },
    [request, snapshot]
  );

  const control = useCallback(
    async (action) => {
      if (action === "reconnect") {
        const version = nextMutationVersion(gatewayMutationVersions, "reconnect");
        const previousGateway = snapshot?.gateway || null;
        setSnapshot((previous) => previous
          ? { ...previous, gateway: { ...previous.gateway, status: "connecting", lastError: null } }
          : previous);
        try {
          return await request("/api/control", { method: "POST", body: { action } });
        } catch (error) {
          if (gatewayMutationVersions.current.get("reconnect") === version) {
            setSnapshot((previous) => previous ? { ...previous, gateway: previousGateway } : previous);
          }
          void clientRef.current?.resync();
          throw error;
        }
      }
      const result = await request("/api/control", { method: "POST", body: { action } });
      if (result?.settings) {
        setSnapshot((previous) =>
          previous ? { ...previous, settings: result.settings } : previous
        );
      }
      return result;
    },
    [request, snapshot]
  );

  const refreshLatency = useCallback(async () => {
    try {
      const result = await request("/api/latency/probe", { method: "POST" });
      if (result?.latency) {
        setSnapshot((previous) => (previous ? { ...previous, latency: result.latency } : previous));
      }
      return result;
    } catch (error) {
      void clientRef.current?.resync();
      throw error;
    }
  }, [request]);

  const fetchLatency = useCallback(async () => {
    const result = await request("/api/latency");
    if (result?.latency) {
      setSnapshot((previous) => (previous ? { ...previous, latency: result.latency } : previous));
    }
    return result;
  }, [request]);

  const fetchSystem = useCallback(async () => {
    const result = await request("/api/system");
    if (result?.system) {
      setSnapshot((previous) => (previous ? { ...previous, system: result.system } : previous));
    }
    return result;
  }, [request]);

  const fetchBenchmark = useCallback(async () => {
    const result = await request("/api/benchmark");
    if (result?.benchmark) {
      setSnapshot((previous) => (previous ? { ...previous, benchmark: result.benchmark } : previous));
    }
    return result;
  }, [request]);

  const previewItem = useCallback(
    (itemId, itemType = "mutation_component") => {
      const params = new URLSearchParams({ itemId, itemType });
      return request(`/api/item-info?${params.toString()}`);
    },
    [request]
  );

  return {
    snapshot,
    logs,
    status,
    latency,
    connected,
    historyRevision,
    authError,
    busy,
    login,
    logout,
    request,
    resync,
    getConnection,
    saveTarget,
    deleteTarget,
    toggleTarget,
    saveSettings,
    refreshSession,
    saveSession,
    clearSession,
    rotateAccessKey,
    resetStats,
    exportBackup,
    exportDatabase,
    importBackup,
    control,
    previewItem,
    refreshLatency,
    fetchLatency,
    fetchSystem,
    fetchBenchmark,
    clearLogs,
    appendLog
  };
}

export {
  ApiError,
  URL_KEY,
  TOKEN_KEY,
  REMEMBER_KEY,
  readStoredToken,
  isRemembered,
  clearStoredToken
};
