import { EventEmitter } from "node:events";

const MAX_BACKOFF_MS = 30_000;
const HEARTBEAT_MS = 25_000;
const HANDSHAKE_TIMEOUT_MS = 15_000;
const USER_AGENT =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

function withToken(rawUrl, token) {
  const url = new URL(rawUrl);
  url.searchParams.set("token", token);
  return url.toString();
}

function decodeMessage(data) {
  if (typeof data === "string") return data;
  if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
    return new TextDecoder().decode(data);
  }
  return null;
}

/**
 * Real Rollercoin market socket. It listens to every frame, exposes all of
 * them as "frame" events, and reconnects with jittered exponential backoff.
 *
 * This uses Bun's native WebSocket client instead of the `ws` package: the
 * ws client goes through node:tls on Bun and can fail the TLS handshake to
 * the upstream edge, while Bun's own client uses the runtime's native TLS
 * stack and supports the custom Origin/User-Agent/Cookie headers the market
 * feed expects.
 */
export class RollercoinGateway extends EventEmitter {
  constructor({ url, getToken, getCookie, logger, latency = null, now = () => Date.now() }) {
    super();
    this.url = url;
    this.getToken = getToken;
    this.getCookie = getCookie;
    this.logger = logger;
    this.latency = latency;
    this.now = now;

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
    this.handshakeTimer = null;
    this.connectGeneration = 0;
    this.connectStartedAt = null;
    this.pingSentAt = null;
    this.lastPingMs = null;
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
    this.#closeSocket(socket, "worker shutdown");
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
    this.#closeSocket(socket, "reconnect");
    this.#setStatus("connecting");
    this.openTimer = setTimeout(() => this.#connect(reason), 150);
  }

  #closeSocket(socket, reason) {
    if (!socket) return;
    try {
      // A close during the opening handshake still emits "error"; keep a
      // listener so shutdown never turns into an uncaught event.
      socket.addEventListener("error", () => {});
      if (socket.readyState === WebSocket.CONNECTING && typeof socket.terminate === "function") {
        socket.terminate();
      } else {
        socket.close(1000, reason);
      }
    } catch {
      /* ignore */
    }
  }

  #clearTimers() {
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
    if (this.openTimer) clearTimeout(this.openTimer);
    if (this.handshakeTimer) clearTimeout(this.handshakeTimer);
    this.reconnectTimer = null;
    this.heartbeatTimer = null;
    this.openTimer = null;
    this.handshakeTimer = null;
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
    this.connectStartedAt = this.now();
    this.emit("status", this.stats());
    this.logger.info("gateway", `connecting to market feed (${reason})`);

    const headers = {
      origin: "https://rollercoin.com",
      referer: "https://rollercoin.com/",
      "user-agent": USER_AGENT
    };
    const cookie = this.getCookie();
    if (cookie) headers.cookie = cookie;

    let socket;
    try {
      socket = new WebSocket(withToken(this.url, token), { headers });
      socket.binaryType = "arraybuffer";
    } catch (error) {
      this.lastError = error?.message || String(error);
      this.logger.error("gateway", `could not create socket: ${this.lastError}`);
      if (this.latency) {
        this.latency.record("wsHandshake", this.now() - this.connectStartedAt, {
          ok: false,
          error: this.lastError
        });
      }
      this.#scheduleReconnect();
      return;
    }

    this.socket = socket;
    this.handshakeTimer = setTimeout(() => {
      if (generation !== this.connectGeneration || this.stopped) return;
      this.lastError = `WebSocket handshake timed out after ${HANDSHAKE_TIMEOUT_MS}ms`;
      this.logger.warn("gateway", this.lastError);
      if (this.latency) {
        this.latency.record("wsHandshake", this.now() - this.connectStartedAt, {
          ok: false,
          error: this.lastError
        });
      }
      this.connectGeneration += 1;
      const pending = this.socket;
      this.socket = null;
      this.#closeSocket(pending, "handshake timeout");
      this.#setStatus("offline");
      this.#scheduleReconnect();
    }, HANDSHAKE_TIMEOUT_MS);
    this.handshakeTimer.unref?.();

    socket.addEventListener("open", () => {
      if (generation !== this.connectGeneration || this.stopped) {
        socket.close(1000, "stale");
        return;
      }
      if (this.handshakeTimer) clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
      this.connectedAt = this.now();
      if (this.latency) {
        this.latency.record("wsHandshake", this.now() - this.connectStartedAt, { ok: true });
      }
      this.attempts = 0;
      this.reconnects += 1;
      this.lastError = null;
      this.#setStatus("online");
      this.logger.info("gateway", "market feed connected");

      this.heartbeatTimer = setInterval(() => {
        if (socket.readyState === WebSocket.OPEN) {
          try {
            this.pingSentAt = this.now();
            socket.ping();
          } catch {
            /* ignore */
          }
        }
      }, HEARTBEAT_MS);
      this.heartbeatTimer.unref?.();
    });

    socket.addEventListener("pong", () => {
      if (generation !== this.connectGeneration) return;
      if (this.pingSentAt === null) return;
      const ms = this.now() - this.pingSentAt;
      this.pingSentAt = null;
      this.lastPingMs = ms;
      if (this.latency) this.latency.record("wsPing", ms, { ok: true });
    });

    socket.addEventListener("message", (event) => {
      if (generation !== this.connectGeneration) return;
      this.lastMessageAt = Date.now();
      const text = decodeMessage(event.data);
      if (text === null) {
        this.parseErrors += 1;
        return;
      }
      let parsed;
      try {
        parsed = JSON.parse(text);
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

    socket.addEventListener("close", (event) => {
      if (generation !== this.connectGeneration) return;
      if (this.handshakeTimer) clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
      const code = Number(event.code) || 1006;
      const reason = typeof event.reason === "string" ? event.reason : "";
      this.socket = null;
      this.connectedAt = null;
      if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
      this.pingSentAt = null;

      if (this.stopped) {
        this.#setStatus("stopped");
        return;
      }

      this.lastError = reason ? `socket closed (${code}: ${reason})` : `socket closed (${code})`;
      // A close during handshake still consumed upstream time; time it.
      if (this.connectStartedAt !== null && this.status === "connecting") {
        if (this.latency) {
          this.latency.record("wsHandshake", this.now() - this.connectStartedAt, {
            ok: false,
            error: this.lastError
          });
        }
        this.connectStartedAt = null;
      }
      this.#setStatus(code === 4001 ? "auth-error" : "offline");
      this.logger.warn("gateway", this.lastError);
      this.#scheduleReconnect();
    });

    socket.addEventListener("error", (event) => {
      if (generation !== this.connectGeneration) return;
      if (this.handshakeTimer) clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
      this.lastError = event?.message || "WebSocket error";
      this.logger.warn("gateway", `socket error: ${this.lastError}`);
      if (this.connectStartedAt !== null && this.status === "connecting" && this.latency) {
        this.latency.record("wsHandshake", this.now() - this.connectStartedAt, {
          ok: false,
          error: this.lastError
        });
        this.connectStartedAt = null;
      }
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
      uptimeMs: this.connectedAt ? this.now() - this.connectedAt : 0,
      lastPingMs: this.lastPingMs
    };
  }
}
