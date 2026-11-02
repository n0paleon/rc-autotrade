import { loadConfig } from "./config.js";
import { createLogger } from "./logger.js";
import { Store } from "./store.js";

/**
 * Emergency access-key reset for a lost key.
 * Stop the worker before running this, otherwise its in-memory state will
 * overwrite the new hash when it shuts down.
 */
const config = loadConfig();
const logger = createLogger({ level: "warn" });
const store = new Store({ dataDir: config.dataDir, logger }).load();

if (config.workerToken) {
  console.error(
    "The access key is pinned by WORKER_TOKEN. Change or remove that environment variable instead."
  );
  process.exit(1);
}

const token = store.rotateWorkerToken();
console.log("");
console.log("  ┌─ NEW WORKER ACCESS KEY ─────────────────────────────────┐");
console.log(`  │ ${token}`);
console.log("  └─────────────────────────────────────────────────────────┘");
console.log("  Restart the worker, then paste this key into the control deck.");
console.log("");
