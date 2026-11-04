import { httpFetch, USER_AGENT } from "./http.js";

function trimBody(text) {
  const clean = String(text ?? "").replace(/\s+/g, " ").trim();
  return clean.length > 240 ? `${clean.slice(0, 240)}…` : clean;
}

async function readJsonResponse(response) {
  const text = await response.text();
  if (!text) return { body: null, text: "" };
  try {
    return { body: JSON.parse(text), text };
  } catch {
    return { body: null, text };
  }
}

/**
 * Thin Rollercoin HTTP client. The worker is intentionally stateless here:
 * cookies and bearer tokens come from the SessionManager so a token rotation
 * only has to happen in one place.
 */
export class RollercoinApi {
  constructor({
    baseUrl,
    session,
    logger,
    timeoutMs = 15_000,
    dispatcher = null,
    fetchImpl = httpFetch
  }) {
    this.baseUrl = baseUrl;
    this.session = session;
    this.logger = logger;
    this.timeoutMs = timeoutMs;
    this.dispatcher = dispatcher;
    this.fetchImpl = fetchImpl;
    this.lastRequestAt = 0;
  }

  /** Timestamp of the most recent upstream call, used by the warm probe. */
  getLastRequestAt() {
    return this.lastRequestAt;
  }

  #touch() {
    this.lastRequestAt = Date.now();
  }

  #headers({ json = true } = {}) {
    const headers = {
      accept: "application/json",
      origin: this.baseUrl,
      referer: `${this.baseUrl}/marketplace`,
      "user-agent": USER_AGENT,
      "accept-language": "en-US,en;q=0.9"
    };
    if (json) headers["content-type"] = "application/json";
    const token = this.session.getAccessToken();
    if (token) headers.authorization = `Bearer ${token}`;
    const cookie = this.session.getCookie();
    if (cookie) headers.cookie = cookie;
    return headers;
  }

  async #request(path, { method = "POST", body, retryOn401 = true } = {}) {
    const url = `${this.baseUrl}${path}`;
    this.#touch();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const response = await this.fetchImpl(url, {
        method,
        headers: this.#headers(),
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
        redirect: "follow",
        ...(this.dispatcher ? { dispatcher: this.dispatcher } : {})
      });
      const { body: parsed, text } = await readJsonResponse(response);

      if (response.status === 401 && retryOn401) {
        const refreshed = await this.session.refresh("http-401", { force: true });
        if (refreshed) return this.#request(path, { method, body, retryOn401: false });
        return {
          ok: false,
          status: 401,
          unauthorized: true,
          error: "Rollercoin rejected the session (401) and the refresh token could not renew it."
        };
      }

      if (!response.ok) {
        const message =
          parsed?.message ||
          parsed?.error ||
          (text.trim().startsWith("<") ? "upstream returned an HTML page" : trimBody(text)) ||
          `HTTP ${response.status}`;
        return { ok: false, status: response.status, body: parsed, error: message };
      }

      if (!parsed) {
        return {
          ok: false,
          status: response.status,
          error: "Upstream returned a non-JSON response (Cloudflare challenge or maintenance?)."
        };
      }

      return { ok: true, status: response.status, body: parsed };
    } catch (error) {
      const message =
        error?.name === "AbortError"
          ? `Upstream request timed out after ${this.timeoutMs}ms.`
          : error?.message || String(error);
      return { ok: false, status: 0, error: message };
    } finally {
      clearTimeout(timer);
    }
  }

  async purchase({ itemId, itemType = "mutation_component", qty, unitPriceRaw }) {
    const payload = {
      challenge: "",
      action: "marketplace",
      itemId,
      itemType,
      totalCount: qty,
      currency: "RLT",
      totalPrice: unitPriceRaw * qty
    };

    const result = await this.#request("/api/marketplace/purchase-item", { body: payload });
    if (!result.ok) return result;

    const orderId =
      result.body?.data?.usersPurchaseOrder ??
      result.body?.data?.orderId ??
      result.body?.usersPurchaseOrder ??
      null;

    if (result.body?.success !== true || !orderId) {
      return {
        ...result,
        ok: false,
        error: result.body?.message || result.body?.error || "Purchase was not accepted by Rollercoin."
      };
    }

    return { ...result, ok: true, orderId: String(orderId) };
  }

  async fetchItemInfo({ itemId, itemType = "mutation_component", currency = "RLT", retryOn401 = true }) {
    const query = new URLSearchParams({ itemId, itemType, currency });
    const url = `${this.baseUrl}/api/marketplace/item-info?${query.toString()}`;
    this.#touch();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 8_000);
    try {
      const response = await this.fetchImpl(url, {
        method: "GET",
        headers: this.#headers(),
        signal: controller.signal,
        redirect: "follow",
        ...(this.dispatcher ? { dispatcher: this.dispatcher } : {})
      });
      const { body, text } = await readJsonResponse(response);
      if (response.status === 401 && retryOn401) {
        const refreshed = await this.session.refresh("item-info-401", { force: true });
        if (refreshed) {
          return this.fetchItemInfo({ itemId, itemType, currency, retryOn401: false });
        }
      }
      if (!response.ok) {
        return {
          ok: false,
          status: response.status,
          error:
            body?.message ||
            body?.error ||
            (text.trim().startsWith("<") ? "item-info returned an HTML page" : trimBody(text)) ||
            `HTTP ${response.status}`
        };
      }
      if (!body?.success || !body?.data) {
        return {
          ok: false,
          status: response.status,
          error: body?.error || "item-info response did not include data."
        };
      }
      return { ok: true, status: response.status, data: body.data };
    } catch (error) {
      return {
        ok: false,
        status: 0,
        error: error?.name === "AbortError" ? "item-info request timed out." : error?.message || String(error)
      };
    } finally {
      clearTimeout(timer);
    }
  }

  async refreshSession(refreshToken) {
    if (!refreshToken) return { ok: false, status: 0, error: "No refresh token available." };
    const url = `${this.baseUrl}/api/auth/refresh`;
    this.#touch();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const headers = this.#headers();
      // The access token is allowed to be expired here; the refresh token and
      // Cloudflare cookie are the credentials that matter.
      delete headers.authorization;
      const response = await this.fetchImpl(url, {
        method: "POST",
        headers,
        body: JSON.stringify({ refresh_token: refreshToken }),
        signal: controller.signal,
        ...(this.dispatcher ? { dispatcher: this.dispatcher } : {})
      });
      const { body, text } = await readJsonResponse(response);
      if (!response.ok) {
        return {
          ok: false,
          status: response.status,
          error:
            body?.message ||
            body?.error ||
            (text.trim().startsWith("<") ? "refresh returned an HTML page" : trimBody(text)) ||
            `HTTP ${response.status}`
        };
      }
      const accessToken = body?.data?.access_token ?? body?.access_token;
      const nextRefresh = body?.data?.refresh_token ?? body?.refresh_token ?? refreshToken;
      if (!accessToken) {
        return { ok: false, status: response.status, error: "Refresh response did not include an access token." };
      }
      return { ok: true, status: response.status, accessToken, refreshToken: nextRefresh };
    } catch (error) {
      return {
        ok: false,
        status: 0,
        error: error?.name === "AbortError" ? "Token refresh timed out." : error?.message || String(error)
      };
    } finally {
      clearTimeout(timer);
    }
  }
}
