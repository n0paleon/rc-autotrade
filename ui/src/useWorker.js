import { useCallback, useEffect, useRef, useState } from "react";
import { WorkerClient, ApiError } from "./lib/client.js";
import { mergeHistory, fixedPrice, formatInt, formatSignedRlt } from "./lib/format.js";
import { sound } from "./lib/sound.js";

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

function messageForError(error) {
  if (error?.status === 401) return "ACCESS DENIED — this key was rejected. Check the worker terminal.";
  if (error?.status === 403) return "ACCESS DENIED — forbidden.";
  if (error?.status === 429) return "TOO MANY ATTEMPTS — wait a minute, then try again.";
  if (error?.status === 0) return error.message;
  return error?.message || "Something went wrong while contacting the worker.";
}

export function useWorker({ notify }) {
  const clientRef = useRef(null);
  const logIdsRef = useRef(new Set());
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
    setSnapshot(data);
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
            history: mergeHistory(runtime.history, data.price, bucket)
          }));
          break;
        }
        case "order": {
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
            setSnapshot((previous) => {
              if (!previous) return previous;
              if (previous.history?.some((item) => item.id === entry.id)) return previous;
              return {
                ...previous,
                history: [entry, ...(previous.history || [])].slice(0, LOOT_LIVE_WINDOW),
                historyTotal: (Number(previous.historyTotal) || 0) + 1
              };
            });
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
              "DRY RUN SIGNAL",
              `${data.label || "ITEM"} ×${formatInt(data.qty)} @ ${fixedPrice(data.unitPrice)} RLT`
            );
          }
          break;
        }
        case "history": {
          if (!data?.id) break;
          setSnapshot((previous) => {
            if (!previous) return previous;
            if (previous.history?.some((item) => item.id === data.id)) return previous;
            return {
              ...previous,
              history: [data, ...(previous.history || [])].slice(0, LOOT_LIVE_WINDOW),
              historyTotal: (Number(previous.historyTotal) || 0) + 1
            };
          });
          break;
        }
        case "stats":
          setSnapshot((previous) => (previous ? { ...previous, stats: data } : previous));
          break;
        case "report":
          setSnapshot((previous) => (previous ? { ...previous, report: data } : previous));
          break;
        case "session":
          setSnapshot((previous) => (previous ? { ...previous, session: data } : previous));
          break;
        case "gateway": {
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
        case "system":
          if (data) {
            setSnapshot((previous) => (previous ? { ...previous, system: data } : previous));
          }
          break;
        case "settings":
          setSnapshot((previous) => (previous ? { ...previous, settings: data } : previous));
          break;
        case "configs":
          if (Array.isArray(data)) {
            setSnapshot((previous) => (previous ? { ...previous, configs: data } : previous));
          }
          break;
        case "history-cleared":
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

  const mergeConfigs = useCallback((configs) => {
    setSnapshot((previous) => (previous ? { ...previous, configs } : previous));
  }, []);

  const saveTarget = useCallback(
    async (target, originalId) => {
      const path = originalId
        ? `/api/configs/${encodeURIComponent(originalId)}`
        : "/api/configs";
      const result = await request(path, { method: originalId ? "PATCH" : "POST", body: target });
      mergeConfigs(result.configs);
      setTimeout(() => clientRef.current?.resync(), 1500);
      return result;
    },
    [mergeConfigs, request]
  );

  const deleteTarget = useCallback(
    async (itemId) => {
      const result = await request(`/api/configs/${encodeURIComponent(itemId)}`, {
        method: "DELETE"
      });
      mergeConfigs(result.configs);
      return result;
    },
    [mergeConfigs, request]
  );

  const toggleTarget = useCallback(
    async (itemId, active) => {
      const result = await request(`/api/configs/${encodeURIComponent(itemId)}`, {
        method: "PATCH",
        body: { active }
      });
      mergeConfigs(result.configs);
      return result;
    },
    [mergeConfigs, request]
  );

  const saveSettings = useCallback(
    async (patch) => {
      const result = await request("/api/settings", { method: "PATCH", body: patch });
      setSnapshot((previous) =>
        previous ? { ...previous, settings: result.settings } : previous
      );
      return result;
    },
    [request]
  );

  const refreshSession = useCallback(async () => {
    const result = await request("/api/session/refresh", { method: "POST" });
    setSnapshot((previous) =>
      previous ? { ...previous, session: result.session } : previous
    );
    return result;
  }, [request]);

  const saveSession = useCallback(
    async (body) => {
      const result = await request("/api/session", { method: "POST", body });
      setSnapshot((previous) =>
        previous ? { ...previous, session: result.session } : previous
      );
      return result;
    },
    [request]
  );

  const clearSession = useCallback(async () => {
    const result = await request("/api/session", { method: "DELETE" });
    setSnapshot((previous) =>
      previous ? { ...previous, session: result.session } : previous
    );
    return result;
  }, [request]);

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
    const result = await request("/api/history", { method: "DELETE" });
    setSnapshot((previous) =>
      previous
        ? {
            ...previous,
            stats: result.stats,
            history: [],
            historyTotal: 0,
            configs: previous.configs.map((config) => ({
              ...config,
              runtime: config.runtime
                ? { ...config.runtime, fills: 0, failures: 0, lastResult: null }
                : config.runtime
            }))
          }
        : previous
    );
    return result;
  }, [request]);

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
      const result = await request(`/api/backup?credentials=${restoreCredentials ? 1 : 0}`, {
        method: "POST",
        body: backup,
        timeout: 300_000
      });
      // The worker broadcasts a fresh snapshot, but resyncing here makes the
      // modal's own counts correct even if a socket frame is missed.
      await clientRef.current?.resync();
      return result;
    },
    [request]
  );

  const control = useCallback(
    async (action) => {
      const result = await request("/api/control", { method: "POST", body: { action } });
      if (result?.settings) {
        setSnapshot((previous) =>
          previous ? { ...previous, settings: result.settings } : previous
        );
      }
      return result;
    },
    [request]
  );

  const refreshLatency = useCallback(async () => {
    const result = await request("/api/latency/probe", { method: "POST" });
    if (result?.latency) {
      setSnapshot((previous) => (previous ? { ...previous, latency: result.latency } : previous));
    }
    return result;
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
