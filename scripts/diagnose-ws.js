#!/usr/bin/env bun
/**
 * Read-only upstream WebSocket diagnostic.
 *
 * Run this in the SAME terminal where `npm run dev` fails:
 *
 *   bun scripts/diagnose-ws.js
 *   bun scripts/diagnose-ws.js --no-token   # TLS only, never opens a session
 *
 * It reads the stored Rollercoin access token from SQLite (read-only) to make
 * the WebSocket probe match the worker, but it never prints the token, never
 * touches targets, and never submits orders. Stop the running worker first:
 * two sockets authenticating with the same session can race.
 */
import { existsSync } from "node:fs";
import { join } from "node:path";
import { lookup, resolve4, resolve6 } from "node:dns/promises";
import net from "node:net";
import tls from "node:tls";
import { loadConfig } from "../src/worker/config.js";
import { openSqlite } from "../src/worker/db.js";
import { redactSecrets } from "../src/shared/redact.js";

const args = new Set(process.argv.slice(2));
const useSession = !args.has("--no-token");
const timeoutMs = 12_000;
const config = loadConfig();
const target = new URL(config.wsUrl || "wss://nws.rollercoin.com/");
const host = target.hostname;
const port = Number(target.port) || (target.protocol === "wss:" ? 443 : 80);

function readStoredToken() {
  const file = join(config.dataDir, "rollercoin.db");
  if (!existsSync(file)) return null;
  let db;
  try {
    db = openSqlite(file, { readonly: true });
    return db.prepare("SELECT access_token FROM session WHERE id = 1").get()?.access_token || null;
  } catch {
    return null;
  } finally {
    try {
      db?.close();
    } catch {
      /* ignore */
    }
  }
}

function describeProxy() {
  const keys = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy"];
  const set = keys.filter((key) => process.env[key]);
  if (set.length === 0) return "none";
  return set
    .map((key) => {
      try {
        const url = new URL(process.env[key]);
        return `${key} -> ${url.protocol}//${url.hostname}${url.port ? `:${url.port}` : ""}`;
      } catch {
        return `${key} -> (unparsable)`;
      }
    })
    .join(", ");
}

function probeTcp(ip) {
  return new Promise((resolve) => {
    const started = Date.now();
    const socket = net.connect({ host: ip, port });
    socket.setTimeout(timeoutMs);
    socket.once("connect", () => {
      socket.destroy();
      resolve({ ok: true, ms: Date.now() - started });
    });
    socket.once("timeout", () => {
      socket.destroy();
      resolve({ ok: false, error: "timeout" });
    });
    socket.once("error", (error) => resolve({ ok: false, error: redactSecrets(error.message) }));
  });
}

/** node:tls is the legacy `ws` stack; useful to separate CA issues from BoringSSL. */
function probeNodeTls(ip) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      resolve(result);
    };
    const socket = tls.connect(
      { host: ip, port, servername: host, rejectUnauthorized: true, ALPNProtocols: ["http/1.1"] },
      () => {
        const cert = socket.getPeerCertificate();
        finish({
          ok: Boolean(socket.authorized),
          authorized: Boolean(socket.authorized),
          authorizationError: socket.authorizationError ? String(socket.authorizationError) : null,
          protocol: socket.getProtocol(),
          cipher: socket.getCipher()?.name,
          issuer: cert?.issuer?.CN || null,
          validTo: cert?.valid_to || null
        });
        socket.end();
      }
    );
    socket.setTimeout(timeoutMs);
    socket.once("timeout", () => {
      socket.destroy();
      finish({ ok: false, error: "timeout" });
    });
    socket.once("error", (error) => finish({ ok: false, error: redactSecrets(error.message) }));
  });
}

async function probeBunFetch() {
  try {
    const response = await fetch(`https://${host}/`, {
      signal: AbortSignal.timeout(timeoutMs),
      redirect: "manual"
    });
    await response.body?.cancel();
    return { ok: true, status: response.status, server: response.headers.get("server") };
  } catch (error) {
    return { ok: false, error: redactSecrets(error?.message || error) };
  }
}

function probeBunWebSocket(token) {
  return new Promise((resolve) => {
    let settled = false;
    let socket;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      try {
        socket?.close();
      } catch {
        /* ignore */
      }
      resolve(result);
    };
    const url = new URL(target);
    url.searchParams.set("token", token);

    try {
      socket = new WebSocket(url.toString(), {
        headers: {
          origin: "https://rollercoin.com",
          referer: "https://rollercoin.com/",
          "user-agent":
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
        }
      });
    } catch (error) {
      finish({ ok: false, error: redactSecrets(error?.message || error) });
      return;
    }

    socket.addEventListener("open", () => finish({ ok: true, status: "open" }));
    socket.addEventListener("error", (event) =>
      finish({ ok: false, error: redactSecrets(event?.message || event?.error?.message || "WebSocket error") })
    );
    setTimeout(() => finish({ ok: false, error: `timeout after ${timeoutMs}ms` }), timeoutMs);
  });
}

function print(label, value) {
  console.log(`${label}: ${typeof value === "string" ? value : JSON.stringify(value)}`);
}

const token = process.env.RC_DIAG_TOKEN || (useSession ? readStoredToken() : null);

console.log("TURU.RC // upstream WebSocket diagnostic");
print("bun", `${process.versions.bun || "?"} (${process.platform}/${process.arch})`);
print("target", `${target.protocol}//${target.host}/`);
print("proxy env", describeProxy());
print("clock", new Date().toISOString());
print("session token", token ? `present (${token.length} chars, not printed)` : "not used");

const addresses = [];
try {
  const [a, aaaa] = await Promise.allSettled([resolve4(host), resolve6(host)]);
  if (a.status === "fulfilled") addresses.push(...a.value.map((address) => ({ address, family: 4 })));
  if (aaaa.status === "fulfilled") addresses.push(...aaaa.value.map((address) => ({ address, family: 6 })));
} catch {
  /* fall through to lookup */
}
if (addresses.length === 0) {
  try {
    for (const entry of await lookup(host, { all: true, verbatim: true })) addresses.push(entry);
  } catch (error) {
    print("dns", { ok: false, error: redactSecrets(error.message) });
  }
}
print("dns", addresses.length > 0 ? addresses : { ok: false, error: "no A/AAAA answer" });

for (const { address } of addresses) {
  print(`tcp ${address}:${port}`, await probeTcp(address));
  print(`node:tls ${address}`, await probeNodeTls(address));
}

print("bun fetch https", await probeBunFetch());
print("bun websocket", await probeBunWebSocket(token || "diagnostic"));

console.log("");
console.log("Interpretation:");
console.log("- token= is sent inside the HTTP upgrade, AFTER TLS; an invalid/expired token gives HTTP 401/403, not 'TLS handshake failed'.");
console.log("- TCP/TLS failures on every address point at this machine's network, firewall, VPN, proxy or CA store.");
console.log("- bun fetch ok + bun websocket TLS failure points at Bun's WebSocket TLS client; compare `bun --revision` with production.");
console.log("- everything ok with a real token means the earlier failure was transient or came from a stale worker process.");
