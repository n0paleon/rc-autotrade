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

  return Object.freeze({
    host,
    port,
    workerToken,
    dataDir: resolve(cwd, env.DATA_DIR || "./data"),
    apiBase: (env.RC_API_BASE || "https://rollercoin.com").replace(/\/+$/, ""),
    wsUrl: env.RC_WS_URL || "wss://nws.rollercoin.com/",
    cookie: (env.RC_COOKIE || "").trim(),
    mock: readBool(env.MOCK),
    logLevel,
    cwd
  });
}
