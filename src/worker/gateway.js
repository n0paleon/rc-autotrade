import { EventEmitter } from "node:events";
import WebSocket from "ws";

const MAX_BACKOFF_MS = 30_000;
const HEARTBEAT_MS = 25_000;
const USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

function withToken(rawUrl, token) {
  const url = new URL(rawUrl);
  url.searchParams.set("token", token);
  return url.toString();
}

/**
 * Real Rollercoin market socket. It listens to every frame, exposes all of
 * them as "frame" events, and reconnects with jittered exponential backoff.
 */
export class RollercoinGateway extends EventEmitter {
  constructor({ url, getToken, getCookie, logger }) {
    super();
    this.url = url;
    this.getToken = getToken;
    this.getCookie = getCookie;
    this.logger = logger;

    this.socket = null;
    this.stopped = true;
    this.status = "idle";
    this.connectedAt = null;
    this.lastMessageAt = null;
    this.lastError = null;
    this.attempts = 0;
    this.reconnects = 0;
    this.eventCount = 0;
    this.parseErrors = 0;
    this.cmdCounts = new Map();
    this.reconnectTimer = null;
    this.heartbeatTimer = null;
    this.openTimer = null;
    this.connectGeneration = 0;
  }

  start() {
    if (!this.stopped) return;
    this.stopped = false;
    this.#connect("startup");
  }

  stop() {
    this.stopped = true;
    this.#clearTimers();
    this.connectGeneration += 1;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      try {
        socket.removeAllListeners();
        socket.close(1000, "worker shutdown");
      } catch {
        /* ignore */
      }
    }
    this.#setStatus("stopped");
  }

  /** Force a fresh socket; used after a session token change. */
  reconnect(reason = "manual") {
    if (this.stopped) return;
    this.logger.info("gateway", `reconnecting: ${reason}`);
    this.#clearTimers();
    this.connectGeneration += 1;
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      try {
        socket.removeAllListeners();
        socket.close(1000, "reconnect");
      } catch {
        /* ignore */
      }
    }
    this.#setStatus("connecting");
    this.openTimer = setTimeout(() => this.#connect(reason), 150);
  }

  #clearTimers() {
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
    if (this.openTimer) clearTimeout(this.openTimer);
    this.reconnectTimer = null;
    this.heartbeatTimer = null;
    this.openTimer = null;
  }

  #setStatus(status) {
    this.status = status;
    this.emit("status", this.stats());
  }

  #connect(reason) {
    if (this.stopped) return;

    const token = this.getToken();
    if (!token) {
      this.status = "auth-missing";
      this.lastError = "No Rollercoin access token stored.";
      this.#setStatus("auth-missing");
      this.logger.warn("gateway", "waiting for a Rollercoin session token (open the Link tab)");
      return;
    }

    this.#clearTimers();
    const generation = ++this.connectGeneration;
    this.attempts += 1;
    this.status = "connecting";
    this.emit("status", this.stats());
    this.logger.info("gateway", `connecting to market feed (${reason})`);

    let socket;
    try {
      const headers = {
        origin: "https://rollercoin.com",
        referer: "https://rollercoin.com/",
        "user-agent": USER_AGENT
      };
      const cookie = this.getCookie();
      if (cookie) headers.cookie = cookie;
      socket = new WebSocket(withToken(this.url, token), {
        headers,
        handshakeTimeout: 15_000,
        maxPayload: 8 * 1024 * 1024
      });
    } catch (error) {
      this.lastError = error?.message || String(error);
      this.logger.error("gateway", `could not create socket: ${this.lastError}`);
      this.#scheduleReconnect();
      return;
    }

    this.socket = socket;

    socket.on("unexpected-response", (request, response) => {
      if (generation !== this.connectGeneration) return;
      const code = response.statusCode;
      this.lastError = `Handshake rejected with HTTP ${code}`;
      this.logger.warn("gateway", this.lastError);
      response.resume();
      request.destroy();
      if (code === 401 || code === 403) {
        this.status = "auth-error";
        this.#setStatus("auth-error");
      }
    });

    socket.on("open", () => {
      if (generation !== this.connectGeneration || this.stopped) {
        socket.close(1000, "stale");
        return;
      }
      this.connectedAt = Date.now();
      this.attempts = 0;
      this.reconnects += 1;
      this.lastError = null;
      this.#setStatus("online");
      this.logger.info("gateway", "market feed connected");

      this.heartbeatTimer = setInterval(() => {
        if (socket.readyState === WebSocket.OPEN) {
          try {
            socket.ping();
          } catch {
            /* ignore */
          }
        }
      }, HEARTBEAT_MS);
      this.heartbeatTimer.unref?.();
    });

    socket.on("message", (data, isBinary) => {
      if (generation !== this.connectGeneration) return;
      this.lastMessageAt = Date.now();
      let parsed;
      try {
        parsed = JSON.parse(isBinary ? data.toString("utf8") : data.toString());
      } catch {
        this.parseErrors += 1;
        return;
      }
      if (!parsed || typeof parsed !== "object") return;

      this.eventCount += 1;
      const cmd = typeof parsed.cmd === "string" ? parsed.cmd : "unknown";
      this.cmdCounts.set(cmd, (this.cmdCounts.get(cmd) || 0) + 1);

      this.emit("frame", { cmd, value: parsed.value, raw: parsed, at: Date.now() });
    });

    socket.on("close", (code, reasonBuffer) => {
      if (generation !== this.connectGeneration) return;
      const reason = reasonBuffer?.toString?.() || "";
      this.socket = null;
      this.connectedAt = null;
      if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;

      if (this.stopped) {
        this.#setStatus("stopped");
        return;
      }

      this.lastError = reason ? `socket closed (${code}: ${reason})` : `socket closed (${code})`;
      this.#setStatus(code === 4001 ? "auth-error" : "offline");
      this.logger.warn("gateway", this.lastError);
      this.#scheduleReconnect();
    });

    socket.on("error", (error) => {
      if (generation !== this.connectGeneration) return;
      this.lastError = error?.message || String(error);
      this.logger.warn("gateway", `socket error: ${this.lastError}`);
    });
  }

  #scheduleReconnect() {
    if (this.stopped || this.reconnectTimer) return;
    const delay = Math.min(MAX_BACKOFF_MS, 1000 * 2 ** Math.min(this.attempts, 5)) + Math.random() * 500;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.#connect("retry");
    }, delay);
    this.logger.info("gateway", `retrying in ${Math.round(delay)}ms`);
  }

  stats() {
    const commands = [...this.cmdCounts.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 15)
      .map(([cmd, count]) => ({ cmd, count }));

    return {
      status: this.status,
      connectedAt: this.connectedAt,
      lastMessageAt: this.lastMessageAt,
      lastError: this.lastError,
      attempts: this.attempts,
      reconnects: this.reconnects,
      eventCount: this.eventCount,
      parseErrors: this.parseErrors,
      commands,
      upstream: new URL(this.url).host,
      uptimeMs: this.connectedAt ? Date.now() - this.connectedAt : 0
    };
  }
}
