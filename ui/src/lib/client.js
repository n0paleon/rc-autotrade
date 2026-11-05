export class ApiError extends Error {
  constructor(message, { status = 0, fields = {} } = {}) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.fields = fields;
  }
}

export function normalizeBaseUrl(raw) {
  let value = String(raw ?? "").trim();
  if (!value) value = window.location.origin;
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(value)) value = `http://${value}`;
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new ApiError("That worker URL is not valid. Example: http://127.0.0.1:8787");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new ApiError("Worker URL must start with http:// or https://.");
  }
  const path = url.pathname.replace(/\/+$/, "");
  return `${url.origin}${path === "/" ? "" : path}`;
}

function wsUrlFor(baseUrl) {
  const url = new URL(baseUrl);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.pathname = `${url.pathname.replace(/\/+$/, "")}/ws`;
  url.search = "";
  url.hash = "";
  return url.toString();
}

async function rawRequest(
  base,
  token,
  path,
  { method = "GET", body, timeout = 12_000, responseType = "json" } = {}
) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeout);
  try {
    const headers = { authorization: `Bearer ${token}`, accept: "application/json" };
    if (body !== undefined) headers["content-type"] = "application/json";
    const response = await fetch(`${base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: controller.signal,
      cache: "no-store"
    });
    if (!response.ok) {
      const text = await response.text();
      let data = {};
      if (text) {
        try {
          data = JSON.parse(text);
        } catch {
          data = {};
        }
      }
      throw new ApiError(data.error || `Request failed (HTTP ${response.status}).`, {
        status: response.status,
        fields: data.fields || {}
      });
    }
    if (responseType === "blob") return await response.blob();
    const text = await response.text();
    if (responseType === "text") return text;
    let data = {};
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = {};
      }
    }
    return data;
  } catch (error) {
    if (error instanceof ApiError) throw error;
    if (error?.name === "AbortError") {
      throw new ApiError("The worker did not answer in time. Is it still running?", { status: 0 });
    }
    throw new ApiError(
      "Cannot reach the worker. Check the URL, make sure it is running, and avoid HTTPS→HTTP mixed content.",
      { status: 0 }
    );
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Transport boundary for the data plane. React never talks to the socket
 * directly; it subscribes to this client's callbacks.
 */
export class WorkerClient {
  constructor({ onSnapshot, onEvent, onStatus, onLatency, onAuthFailure, onLogs } = {}) {
    this.onSnapshot = onSnapshot;
    this.onEvent = onEvent;
    this.onStatus = onStatus;
    this.onLatency = onLatency;
    this.onAuthFailure = onAuthFailure;
    this.onLogs = onLogs;

    this.baseUrl = "";
    this.token = "";
    this.socket = null;
    this.status = "idle";
    this.manualClose = false;
    this.reconnectAttempt = 0;
    this.reconnectTimer = null;
    this.pingTimer = null;
    this.disposed = false;
  }

  async login(rawUrl, token) {
    const base = normalizeBaseUrl(rawUrl);
    if (!token) throw new ApiError("Access key required.");
    if (
      window.location.protocol === "https:" &&
      base.startsWith("http://") &&
      !/^http:\/\/(127\.0\.0\.1|localhost|\[::1\])(:|\/|$)/.test(base)
    ) {
      throw new ApiError(
        "This page is HTTPS but the worker is HTTP. Use a localhost URL or put the worker behind HTTPS."
      );
    }
    const state = await rawRequest(base, token, "/api/state");
    this.baseUrl = base;
    this.token = token;
    this.manualClose = false;
    this.disposed = false;
    this.connectSocket();
    return state;
  }

  get isAuthenticated() {
    return Boolean(this.baseUrl && this.token);
  }

  async request(path, options = {}) {
    return this.#requestTyped(path, "json", options);
  }

  /** Fetches a non-JSON download (backup file) as text. */
  async requestText(path, options = {}) {
    return this.#requestTyped(path, "text", options);
  }

  /** Fetches a binary download (SQLite snapshot) as a Blob. */
  async requestBlob(path, options = {}) {
    return this.#requestTyped(path, "blob", options);
  }

  async #requestTyped(path, responseType, options) {
    try {
      return await rawRequest(this.baseUrl, this.token, path, { ...options, responseType });
    } catch (error) {
      if (error.status === 401) {
        this.onAuthFailure?.("ACCESS KEY REJECTED — it may have been rotated. Paste the current key.");
      }
      throw error;
    }
  }

  async resync() {
    if (!this.isAuthenticated) return null;
    try {
      const state = await rawRequest(this.baseUrl, this.token, "/api/state");
      this.onSnapshot?.(state);
      return state;
    } catch (error) {
      if (error.status === 401) {
        this.onAuthFailure?.("ACCESS KEY REJECTED — it may have been rotated. Paste the current key.");
      }
      return null;
    }
  }

  connectSocket() {
    if (this.disposed || !this.isAuthenticated) return;
    this.#closeSocket();
    this.manualClose = false;
    this.#setStatus("connecting");

    let url;
    try {
      url = wsUrlFor(this.baseUrl);
    } catch {
      this.#setStatus("offline");
      return;
    }

    const safeProtocol = /^[A-Za-z0-9._~-]{1,123}$/.test(this.token);
    if (!safeProtocol) url += `?token=${encodeURIComponent(this.token)}`;
    const protocols = safeProtocol ? ["rcw.v1", this.token] : ["rcw.v1"];

    let socket;
    try {
      socket = new WebSocket(url, protocols);
    } catch {
      this.#setStatus("offline");
      this.#scheduleReconnect();
      return;
    }
    this.socket = socket;

    socket.addEventListener("open", () => {
      if (this.socket !== socket || this.disposed) return;
      this.reconnectAttempt = 0;
      if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
      this.#setStatus("online");
      this.#startPing();
      this.resync();
    });

    socket.addEventListener("message", (event) => {
      if (this.socket !== socket) return;
      let message;
      try {
        message = JSON.parse(event.data);
      } catch {
        return;
      }
      if (message.type === "hello") {
        if (Array.isArray(message.data?.recentLogs)) this.onLogs?.(message.data.recentLogs);
      } else if (message.type === "snapshot") {
        this.onSnapshot?.(message.data);
      } else if (message.type === "pong") {
        if (message.data?.t) this.onLatency?.(Math.max(0, Date.now() - message.data.t));
      } else {
        this.onEvent?.(message.type, message.data, message.at);
      }
    });

    socket.addEventListener("close", (event) => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.#stopPing();
      this.#setStatus("offline");
      if (this.manualClose || this.disposed) return;
      if (event.code === 4003) {
        this.reconnectAttempt = 0;
        this.#scheduleReconnect(250);
      } else if (event.code === 4001) {
        // The worker accepted the socket but rejected the key. Retrying would
        // just hammer the auth limiter, so stop and surface the login error.
        this.manualClose = true;
        this.#setStatus("auth-error");
        this.onAuthFailure?.("ACCESS KEY REJECTED — it may have been rotated. Paste the current key.");
      } else {
        this.#scheduleReconnect();
      }
    });

    socket.addEventListener("error", () => {
      /* close handles retry */
    });
  }

  replaceToken(token) {
    this.token = token;
    if (this.socket) this.connectSocket();
  }

  #closeSocket() {
    if (!this.socket) return;
    this.manualClose = true;
    try {
      this.socket.close(1000, "reconnect");
    } catch {
      /* ignore */
    }
    this.socket = null;
  }

  #setStatus(status) {
    this.status = status;
    this.onStatus?.(status);
  }

  #scheduleReconnect(fixedDelay = null) {
    if (this.disposed || !this.isAuthenticated || this.reconnectTimer) return;
    const attempt = this.reconnectAttempt++;
    const delay =
      fixedDelay ?? Math.min(15_000, 800 * 1.7 ** Math.min(attempt, 6)) + Math.random() * 300;
    this.#setStatus("connecting");
    this.reconnectTimer = setTimeout(async () => {
      this.reconnectTimer = null;
      if (this.disposed || !this.isAuthenticated) return;
      if (navigator.onLine === false) {
        this.#scheduleReconnect();
        return;
      }
      await this.resync();
      this.connectSocket();
    }, delay);
  }

  #startPing() {
    this.#stopPing();
    const ping = () => {
      if (this.socket?.readyState === WebSocket.OPEN) {
        try {
          this.socket.send(JSON.stringify({ type: "ping", t: Date.now() }));
        } catch {
          /* ignore */
        }
      }
    };
    ping();
    this.pingTimer = setInterval(ping, 10_000);
  }

  #stopPing() {
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.pingTimer = null;
  }

  logout() {
    this.manualClose = true;
    this.disposed = true;
    this.#stopPing();
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.#closeSocket();
    this.socket = null;
    this.token = "";
    this.baseUrl = "";
    this.#setStatus("idle");
  }

  dispose() {
    this.manualClose = true;
    this.disposed = true;
    this.#stopPing();
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.#closeSocket();
  }
}
