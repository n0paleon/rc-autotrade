import { LOG_BUFFER_SIZE } from "../shared/constants.js";

const LEVEL_WEIGHT = { debug: 10, info: 20, warn: 30, error: 40 };
const COLORS = {
  debug: "\x1b[38;5;245m",
  info: "\x1b[38;5;81m",
  warn: "\x1b[38;5;220m",
  error: "\x1b[38;5;203m",
  scope: "\x1b[38;5;177m",
  dim: "\x1b[2m",
  reset: "\x1b[0m"
};

function timestamp(date = new Date()) {
  return date.toLocaleTimeString("en-GB", { hour12: false });
}

/**
 * Console + in-memory + socket logger.
 * Never pass tokens or cookies into `message` or `data`.
 */
export function createLogger({ level = "info", color = process.stdout.isTTY } = {}) {
  const threshold = LEVEL_WEIGHT[level] ?? 20;
  const buffer = [];
  let nextId = 1;
  let sink = null;

  function emit(levelName, scope, message, data) {
    if ((LEVEL_WEIGHT[levelName] ?? 20) < threshold) return;

    const entry = {
      id: nextId++,
      at: Date.now(),
      level: levelName,
      scope,
      message: String(message),
      ...(data && typeof data === "object" ? { data } : {})
    };

    buffer.push(entry);
    if (buffer.length > LOG_BUFFER_SIZE) buffer.splice(0, buffer.length - LOG_BUFFER_SIZE);

    const paint = color
      ? `${COLORS[levelName]}${levelName.toUpperCase().padEnd(5)}${COLORS.reset}`
      : levelName.toUpperCase().padEnd(5);
    const scopeText = color ? `${COLORS.scope}${scope.padEnd(8)}${COLORS.reset}` : scope.padEnd(8);
    const line = `${color ? COLORS.dim : ""}${timestamp()}${color ? COLORS.reset : ""} ${paint} ${scopeText} ${message}`;
    const stream = levelName === "error" || levelName === "warn" ? console.error : console.log;
    stream(line);
    if (data && level === "debug") stream(data);

    try {
      sink?.(entry);
    } catch {
      /* a broken client must never crash the worker */
    }
  }

  return {
    debug: (scope, message, data) => emit("debug", scope, message, data),
    info: (scope, message, data) => emit("info", scope, message, data),
    warn: (scope, message, data) => emit("warn", scope, message, data),
    error: (scope, message, data) => emit("error", scope, message, data),
    setSink(fn) {
      sink = typeof fn === "function" ? fn : null;
    },
    recent(limit = 100) {
      return buffer.slice(-limit);
    },
    setLevel(nextLevel) {
      if (!(nextLevel in LEVEL_WEIGHT)) return;
      level = nextLevel;
    }
  };
}
