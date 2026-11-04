import { EventEmitter } from "node:events";

const REFRESH_AHEAD_MS = 2 * 60 * 1000;
const MIN_REFRESH_INTERVAL_MS = 30 * 1000;
const FALLBACK_REFRESH_MS = 25 * 60 * 1000;
const FAILURE_COOLDOWN_MS = 60 * 1000;
const TICK_MS = 30 * 1000;

/**
 * Owns the Rollercoin session pair and keeps it fresh.
 *
 * - Refreshes ahead of JWT expiry (or every 25 min for opaque tokens).
 * - Coalesces concurrent refreshes.
 * - Emits "updated" whenever credentials change so the socket and UI follow.
 */
export class SessionManager extends EventEmitter {
  constructor({ store, api, logger }) {
    super();
    this.store = store;
    this.api = api;
    this.logger = logger;
    this.timer = null;
    this.inflight = null;
    this.lastAttemptAt = null;
    this.lastFailureAt = null;
    this.refreshing = false;
    this.generation = 0;
  }

  start() {
    if (this.timer) return;
    this.timer = setInterval(() => this.tick(), TICK_MS);
    this.timer.unref?.();
    this.tick();
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  getAccessToken() {
    return this.store.state.session.accessToken;
  }

  getRefreshToken() {
    return this.store.state.session.refreshToken;
  }

  getCookie() {
    return this.store.state.session.cookie;
  }

  set(patch) {
    const previous = this.getAccessToken();
    const clearing = patch.clearAccessToken === true || patch.clearRefreshToken === true;
    // A refresh that was already in flight must not resurrect credentials the
    // user just removed. Bump the generation so its result is discarded.
    if (clearing) this.generation += 1;

    this.store.setSession(patch);
    const next = this.getAccessToken();
    this.lastFailureAt = null;
    this.logger.info(
      "session",
      clearing
        ? "session credentials cleared"
        : previous && next && previous !== next
          ? "session token replaced"
          : "session settings updated"
    );
    const reason = patch.source === "bridge" ? "bridge" : "manual";
    this.emit("updated", { reason, changed: previous !== next, status: this.status() });
    return this.status();
  }

  /** Remove the Rollercoin access token, refresh token and cookie. */
  clear() {
    return this.set({
      clearAccessToken: true,
      clearRefreshToken: true,
      clearCookie: true,
      source: "ui"
    });
  }

  needsRefresh(now = Date.now()) {
    const session = this.store.state.session;
    if (!session.accessToken || !session.refreshToken) return false;

    const expiresAt = this.store.sessionStatus().expiresAt;
    if (expiresAt) {
      if (expiresAt - now > REFRESH_AHEAD_MS) return false;
      // Rollercoin issues 10-minute access tokens. Refreshing based on
      // "expires within 10 minutes" means refreshing immediately, every tick.
      // Keep a small guard for tokens that are already close to expiry.
      const anchor = session.lastRefreshAt || 0;
      return now - anchor >= MIN_REFRESH_INTERVAL_MS;
    }

    const anchor = session.lastRefreshAt || session.updatedAt || 0;
    return now - anchor >= FALLBACK_REFRESH_MS;
  }

  tick() {
    if (!this.store.state.settings.autoRefresh) return;
    if (!this.needsRefresh()) return;
    this.refresh("scheduled").catch(() => {});
  }

  /**
   * @param {string} reason
   * @param {{force?: boolean}} [opts]
   * @returns {Promise<boolean>}
   */
  async refresh(reason = "manual", { force = false } = {}) {
    if (this.inflight) return this.inflight;

    const refreshToken = this.getRefreshToken();
    if (!refreshToken) {
      this.store.markSessionError("No refresh token stored. Paste a fresh session.");
      this.emit("updated", { reason: "missing-refresh", status: this.status() });
      return false;
    }

    const now = Date.now();
    if (!force && this.lastFailureAt && now - this.lastFailureAt < FAILURE_COOLDOWN_MS) {
      return false;
    }

    this.lastAttemptAt = now;
    this.refreshing = true;
    const generation = this.generation;
    this.logger.info("session", `refreshing access token (${reason})`);

    this.inflight = (async () => {
      try {
        const result = await this.api.refreshSession(refreshToken);
        if (generation !== this.generation) {
          this.logger.info("session", "discarding refresh result after session reset");
          return false;
        }
        if (!result.ok) {
          this.lastFailureAt = Date.now();
          this.store.markSessionError(result.error || "Token refresh failed.");
          this.logger.warn("session", `refresh failed: ${result.error}`);
          this.emit("updated", { reason: "refresh-failed", changed: false, status: this.status() });
          return false;
        }

        const before = this.getAccessToken();
        this.store.markRefreshed({
          accessToken: result.accessToken,
          refreshToken: result.refreshToken
        });
        this.lastFailureAt = null;
        this.logger.info("session", "access token renewed");
        this.emit("updated", {
          reason: "refresh",
          changed: before !== result.accessToken,
          status: this.status()
        });
        return true;
      } catch (error) {
        if (generation !== this.generation) {
          this.logger.info("session", "discarding refresh failure after session reset");
          return false;
        }
        this.lastFailureAt = Date.now();
        this.store.markSessionError(error?.message || String(error));
        this.logger.error("session", `refresh crashed: ${error?.message || error}`);
        this.emit("updated", { reason: "refresh-failed", changed: false, status: this.status() });
        return false;
      } finally {
        this.refreshing = false;
        this.inflight = null;
      }
    })();

    return this.inflight;
  }

  status() {
    const base = this.store.sessionStatus();
    return {
      ...base,
      autoRefresh: Boolean(this.store.state.settings.autoRefresh),
      needsRefresh: this.needsRefresh(),
      refreshing: this.refreshing,
      lastAttemptAt: this.lastAttemptAt,
      lastFailureAt: this.lastFailureAt
    };
  }
}
