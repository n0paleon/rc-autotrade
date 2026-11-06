import { EventEmitter } from "node:events";
import { httpFetch, USER_AGENT } from "./http.js";

const DEFAULT_TIMEOUT_MS = 10_000;
const DEFAULT_SUSPEND_MS = 5 * 60_000;
const MAX_BACKOFF_MS = 5 * 60_000;

function jittered(ms, random, ratio) {
  const spread = ms * ratio;
  return Math.max(1, Math.round(ms - spread + random() * spread * 2));
}

/**
 * Keeps the Rollercoin edge connection warm while the sniper is armed.
 *
 * The probe sends an empty POST to the purchase endpoint and uses the same
 * Bun-native fetch pool as purchases. Any HTTP response confirms the request
 * reached upstream; it is skipped whenever real traffic happened recently,
 * backs off on transport failures, and pauses on 429 responses.
 */
export class KeepaliveService extends EventEmitter {
  constructor({
    url,
    intervalMs = 30_000,
    api = null,
    logger = null,
    fetchImpl = httpFetch,
    shouldProbe = () => true,
    now = () => Date.now(),
    random = Math.random,
    timeoutMs = DEFAULT_TIMEOUT_MS,
    suspendMs = DEFAULT_SUSPEND_MS,
    jitterRatio = 0.1,
    latency = null
  }) {
    super();
    const interval = Number(intervalMs);
    this.url = typeof url === "string" ? url.trim() : "";
    this.intervalMs = Number.isFinite(interval) && interval > 0 ? interval : 0;
    this.enabled = Boolean(this.url) && this.intervalMs > 0;
    this.api = api;
    this.logger = logger;
    this.fetchImpl = fetchImpl;
    this.latency = latency;
    this.shouldProbe = shouldProbe;
    this.now = now;
    this.random = random;
    this.timeoutMs = Math.max(1_000, Number(timeoutMs) || DEFAULT_TIMEOUT_MS);
    this.suspendMs = Math.max(1_000, Number(suspendMs) || DEFAULT_SUSPEND_MS);
    this.jitterRatio = Math.min(0.5, Math.max(0, Number(jitterRatio) || 0));
    this.maxBackoffMs = Math.max(this.intervalMs, MAX_BACKOFF_MS);

    this.stopped = true;
    this.stopping = false;
    this.timer = null;
    this.inflight = null;
    this.controller = null;

    this.probes = 0;
    this.failures = 0;
    this.consecutiveFailures = 0;
    this.lastAttemptAt = null;
    this.lastSuccessAt = null;
    this.lastSkipAt = null;
    this.lastStatus = null;
    this.lastRttMs = null;
    this.lastError = null;
    this.nextProbeAt = null;
    this.suspendedUntil = null;
  }

  start() {
    if (!this.enabled) {
      this.logger?.debug("keepalive", "warm probe disabled");
      return;
    }
    if (!this.stopped) return;
    this.stopped = false;
    this.stopping = false;
    this.logger?.info("keepalive", `probing ${this.url} every ~${Math.round(this.intervalMs)}ms`);
    this.#schedule(this.intervalMs);
  }

  stop() {
    this.stopped = true;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.nextProbeAt = null;
    if (this.controller) {
      this.stopping = true;
      try {
        this.controller.abort();
      } catch {
        /* ignore */
      }
    }
    this.controller = null;
  }

  /** Run a probe immediately instead of waiting for the next slot. */
  wake(reason = "manual") {
    if (!this.enabled || this.stopped) return;
    this.logger?.debug("keepalive", `wake: ${reason}`);
    this.#schedule(0, { jitter: false });
  }

  probeOnce() {
    if (!this.enabled) return Promise.resolve({ ok: false, skipped: true, reason: "disabled" });
    if (this.inflight) return this.inflight;
    this.inflight = this.#probe().finally(() => {
      this.inflight = null;
    });
    return this.inflight;
  }

  stats() {
    return {
      enabled: this.enabled,
      running: !this.stopped,
      url: this.url,
      intervalMs: this.intervalMs,
      timeoutMs: this.timeoutMs,
      probes: this.probes,
      failures: this.failures,
      consecutiveFailures: this.consecutiveFailures,
      lastAttemptAt: this.lastAttemptAt,
      lastSuccessAt: this.lastSuccessAt,
      lastSkipAt: this.lastSkipAt,
      lastStatus: this.lastStatus,
      lastRttMs: this.lastRttMs,
      lastError: this.lastError,
      nextProbeAt: this.nextProbeAt,
      suspendedUntil: this.suspendedUntil
    };
  }

  async #tick() {
    if (this.stopped) return;
    const now = this.now();

    if (this.suspendedUntil) {
      if (now < this.suspendedUntil) {
        this.#schedule(this.suspendedUntil - now, { jitter: false });
        return;
      }
      this.suspendedUntil = null;
    }

    let allowed = true;
    try {
      allowed = Boolean(this.shouldProbe());
    } catch (error) {
      this.logger?.warn("keepalive", `shouldProbe failed: ${error?.message || error}`);
    }
    if (!allowed) {
      this.lastSkipAt = now;
      this.#schedule(this.intervalMs);
      return;
    }

    const lastReal = this.#lastRealRequestAt();
    const sinceReal = lastReal ? now - lastReal : Number.POSITIVE_INFINITY;
    if (sinceReal < this.intervalMs * 0.9) {
      // Real traffic already keeps the socket warm; do not add noise.
      this.lastSkipAt = now;
      this.#schedule(Math.max(1_000, this.intervalMs - sinceReal));
      return;
    }

    const result = await this.probeOnce();
    if (this.stopped) return;

    if (result.suspendedUntil) {
      this.#schedule(Math.max(1_000, result.suspendedUntil - this.now()), { jitter: false });
      return;
    }
    if (result.ok) {
      this.#schedule(this.intervalMs);
      return;
    }

    const backoff = this.intervalMs * 2 ** Math.min(this.consecutiveFailures, 6);
    this.#schedule(Math.min(this.maxBackoffMs, backoff));
  }

  async #probe() {
    const startedAt = this.now();
    this.probes += 1;
    this.lastAttemptAt = startedAt;

    let origin;
    try {
      origin = new URL(this.url).origin;
    } catch {
      origin = "";
    }

    const controller = new AbortController();
    this.controller = controller;
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    timer.unref?.();

    try {
      const headers = {
        accept: "application/json",
        "content-type": "application/json",
        ...(origin ? { origin, referer: `${origin}/marketplace` } : {}),
        "user-agent": USER_AGENT,
        "accept-language": "en-US,en;q=0.9"
      };
      const token = this.api?.session?.getAccessToken?.();
      if (token) headers.authorization = `Bearer ${token}`;
      const cookie = this.api?.session?.getCookie?.();
      if (cookie) headers.cookie = cookie;

      const response = await this.#fetch(this.url, {
        method: "POST",
        headers,
        // Empty purchase data can only be rejected by validation; it cannot
        // place an order, while still exercising the same authenticated route.
        body: "{}",
        signal: controller.signal,
        redirect: "follow"
      });
      this.lastStatus = response.status;

      // A response of any HTTP status means the request reached upstream. Drain
      // its body so Bun can reuse the connection from its pool when possible.
      try {
        await response.arrayBuffer();
      } catch {
        // The response headers already confirm dispatch; an unreadable body
        // only means the socket may not be reusable.
      }

      const completedAt = this.now();
      const rttMs = Math.max(0, completedAt - startedAt);
      const suspendedUntil = response.status === 429 ? completedAt + this.suspendMs : null;
      this.lastSuccessAt = completedAt;
      this.lastRttMs = rttMs;
      this.consecutiveFailures = 0;
      this.lastError = null;
      this.suspendedUntil = suspendedUntil;
      if (suspendedUntil) {
        this.logger?.warn(
          "keepalive",
          `HTTP 429; pausing probes until ${new Date(suspendedUntil).toISOString()}`
        );
      } else {
        this.logger?.debug("keepalive", `warm HTTP ${response.status} in ${rttMs}ms`);
      }
      return { ok: true, status: response.status, rttMs, suspendedUntil };
    } catch (error) {
      if (this.stopping) return { ok: false, aborted: true };
      const message =
        error?.name === "AbortError"
          ? `timed out after ${this.timeoutMs}ms`
          : error?.message || String(error);
      this.#recordFailure(message);
      return { ok: false, status: 0, error: message };
    } finally {
      clearTimeout(timer);
      if (this.controller === controller) this.controller = null;
    }
  }

  #recordFailure(message, suspendedUntil = null) {
    this.failures += 1;
    this.consecutiveFailures += 1;
    this.lastError = message;

    if (suspendedUntil) {
      this.suspendedUntil = suspendedUntil;
      this.logger?.warn(
        "keepalive",
        `${message}; pausing probes until ${new Date(suspendedUntil).toISOString()}`
      );
      return;
    }

    if (this.consecutiveFailures === 1 || this.consecutiveFailures % 10 === 0) {
      this.logger?.warn("keepalive", `probe failed: ${message} (${this.consecutiveFailures}x)`);
    }
  }

  /** Upstream probe fetch timed as a `keepalive` latency sample. */
  #fetch(url, init) {
    if (!this.latency) return this.fetchImpl(url, init);
    return this.latency.measure("keepalive", () => this.fetchImpl(url, init), { okFn: () => true });
  }

  #lastRealRequestAt() {
    const value =
      typeof this.api?.getLastRequestAt === "function"
        ? this.api.getLastRequestAt()
        : this.api?.lastRequestAt;
    const number = Number(value);
    return Number.isFinite(number) && number > 0 ? number : 0;
  }

  #schedule(delay, { jitter = true } = {}) {
    if (this.stopped || !this.enabled) return;
    if (this.timer) clearTimeout(this.timer);
    const wait = jitter
      ? jittered(delay, this.random, this.jitterRatio)
      : Math.max(0, Math.round(delay));
    this.nextProbeAt = this.now() + wait;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.nextProbeAt = null;
      this.#tick().catch((error) => {
        this.logger?.warn("keepalive", `tick crashed: ${error?.message || error}`);
        this.#schedule(this.intervalMs);
      });
    }, wait);
    this.timer.unref?.();
  }
}
