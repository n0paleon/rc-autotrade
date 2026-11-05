import { spawn } from "node:child_process";
import net from "node:net";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const workerEntry = fileURLToPath(new URL("../src/worker/index.js", import.meta.url));
const viteEntry = fileURLToPath(new URL("../node_modules/vite/bin/vite.js", import.meta.url));

// The worker has to run on Bun (bun:sqlite). Vite must *not*: its dev proxy calls
// net.Socket#destroySoon(), which Bun's node:http upgrade socket does not implement,
// so a proxied WS request that fails (worker not listening yet, or a plain HTTP
// reply to an upgrade) kills the dev server with
// "TypeError: socket.destroySoon is not a function".
const WORKER_RUNTIME = process.execPath;
const VITE_RUNTIME = process.env.VITE_NODE_BIN || "node";

const WORKER_PORT = Number(process.env.PORT) || 8787;
const WORKER_HOST = "127.0.0.1";

const children = [];
let shuttingDown = false;

function shutdown(code) {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const child of children) {
    if (!child.killed) {
      try {
        child.kill("SIGTERM");
      } catch {
        /* already gone */
      }
    }
  }
  setTimeout(() => process.exit(code), 200).unref?.();
}

function start(name, args, command = WORKER_RUNTIME) {
  const child = spawn(command, args, {
    cwd: root,
    stdio: "inherit",
    env: process.env
  });
  children.push(child);

  child.on("exit", (code, signal) => {
    if (shuttingDown) return;
    const reason = signal ? `signal ${signal}` : `code ${code}`;
    console.error(`\n[dev] ${name} exited (${reason}) — stopping the dev session.`);
    shutdown(code ?? 1);
  });

  child.on("error", (error) => {
    console.error(`[dev] failed to start ${name}: ${error.message}`);
    shutdown(1);
  });

  return child;
}

/** Resolve once the worker accepts TCP connections, so the first WS upgrade isn't proxied into a closed port. */
function waitForPort(port, host, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve) => {
    const attempt = () => {
      const socket = net.connect({ port, host });
      socket.setTimeout(1_000);
      socket.once("connect", () => {
        socket.destroy();
        resolve(true);
      });
      const retry = () => {
        socket.destroy();
        if (Date.now() > deadline) resolve(false);
        else setTimeout(attempt, 150);
      };
      socket.once("error", retry);
      socket.once("timeout", retry);
    };
    attempt();
  });
}

process.on("SIGINT", () => shutdown(0));
process.on("SIGTERM", () => shutdown(0));

start("worker", ["--watch", workerEntry]);

const workerReady = await waitForPort(WORKER_PORT, WORKER_HOST);
if (shuttingDown) process.exit(0);
if (workerReady) {
  console.log(`[dev] worker listening on ${WORKER_HOST}:${WORKER_PORT} — starting vite`);
} else {
  console.warn(
    `[dev] worker did not open ${WORKER_HOST}:${WORKER_PORT} within 20s — starting vite anyway ` +
      `(WS proxy requests will fail until it does)`
  );
}

start("vite", [viteEntry], VITE_RUNTIME);
