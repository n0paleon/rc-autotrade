import { existsSync } from "node:fs";
import { resolve } from "node:path";

function readInt(value, fallback) {
  const parsed = Number.parseInt(String(value ?? ""), 10);
  return Number.isInteger(parsed) ? parsed : fallback;
}

function readBool(value, fallback = false) {
  if (value === undefined || value === null || value === "") return fallback;
  return ["1", "true", "yes", "on"].includes(String(value).toLowerCase());
}

/**
 * Loads .env (without adding a dependency) and exposes a frozen config object.
 */
export function loadConfig({ cwd = process.cwd(), env = process.env } = {}) {
  const envFile = resolve(cwd, ".env");
  if (existsSync(envFile) && typeof process.loadEnvFile === "function") {
    try {
      process.loadEnvFile(envFile);
    } catch (error) {
      console.warn(`[config] could not load ${envFile}: ${error.message}`);
    }
  }

  const port = readInt(env.PORT, 8787);
  if (port < 0 || port > 65_535) throw new Error(`PORT must be between 0 and 65535 (got ${env.PORT}).`);

  const host = (env.HOST || "127.0.0.1").trim();
  const workerToken = (env.WORKER_TOKEN || "").trim();
  if (workerToken && workerToken.length < 16) {
    throw new Error("WORKER_TOKEN must be at least 16 characters long.");
  }

  const logLevel = (env.LOG_LEVEL || "info").toLowerCase();
  if (!["debug", "info", "warn", "error"].includes(logLevel)) {
    throw new Error(`LOG_LEVEL must be debug, info, warn or error (got ${env.LOG_LEVEL}).`);
  }

  const apiBase = (env.RC_API_BASE || "https://rollercoin.com").replace(/\/+$/, "");
  const keepAliveUrlInput = (env.RC_KEEPALIVE_URL || "").trim();
  let keepAliveUrl = `${apiBase}/api/events/config`;
  if (keepAliveUrlInput) {
    try {
      keepAliveUrl = new URL(keepAliveUrlInput, `${apiBase}/`).toString();
    } catch {
      throw new Error(`RC_KEEPALIVE_URL must be a valid URL (got ${keepAliveUrlInput}).`);
    }
  }

  // Bun's fetch pool is managed by the runtime; only the warm-probe interval
  // is configurable. RC_KEEPALIVE_MS=0 is the explicit opt-out.
  const keepAliveMs = readInt(env.RC_KEEPALIVE_MS, 30_000);

  // Worker health sampler. Missing/non-numeric values fall back to 2s;
  // explicit values are clamped to 500ms..60s so the deck can never be
  // starved or spammed.
  const systemSampleMs = readInt(env.SYSTEM_SAMPLE_MS, 2_000);

  return Object.freeze({
    host,
    port,
    workerToken,
    dataDir: resolve(cwd, env.DATA_DIR || "./data"),
    apiBase,
    wsUrl: env.RC_WS_URL || "wss://nws.rollercoin.com/",
    cookie: (env.RC_COOKIE || "").trim(),
    keepAliveMs:
      keepAliveMs <= 0 ? 0 : Math.min(600_000, Math.max(5_000, keepAliveMs)),
    keepAliveUrl,
    systemSampleMs: Math.min(60_000, Math.max(500, systemSampleMs)),
    mock: readBool(env.MOCK),
    logLevel,
    cwd
  });
}
