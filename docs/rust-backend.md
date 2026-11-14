# Rust backend — architecture & porting notes

The backend is a single Rust crate at the repository root (`src/`, `examples/`,
`tests/`). It replaced the Bun/JavaScript worker: the React deck (`ui/` → `dist/`)
and the `rcw.v1` control protocol did not change, so the UI was reused as-is.

```bash
mise install                # Rust 1.99.1, pinned in mise.toml
bun install && bun run build # deck → dist/
cargo run --release          # backend at http://127.0.0.1:8787
# or: bun start  (build deck + release binary)
```

---

## 1. Toolchain

`mise.toml` pins the exact toolchain and keeps the crate cache inside the repo:

```toml
[tools]
rust = "1.99.1"
bun = "1.4.4"
node = "24.16.0"

[env]
CARGO_HOME = "{{config_root}}/.cargo-home"
```

- `mise install` once, then `mise exec -- cargo ...` (or `mise activate`).
- `.cargo-home/` and `target/` are gitignored.
- The `CARGO_HOME` override exists because the agent sandbox may not write to
  `~/.cargo`; on a normal machine plain `cargo` works too.

## 2. Running and deploying

| Mode | Command | Port | Data |
| --- | --- | --- | --- |
| development (debug) | `bun run dev` (cargo run + Vite) | 8787 | `./data` |
| backend only, debug | `cargo run` | 8787 | `./data` |
| release | `bun start` (`bun run build` + `cargo run --release`) | 8787 | `./data` |
| simulator | `bun run mock` | 8787 | `./data` |

`PORT` and `RC_DATA_DIR` override the defaults:
`RC_DATA_DIR=./data-alt PORT=9000 cargo run --release`. The binary resolves
`dist/` and `.env` relative to the working directory, so run it from the repo
root.

Deploy: `bun run build && cargo build --release`, then run
`./target/release/rollercoin-worker`. PM2 uses `ecosystem.config.cjs`
(`interpreter: "none"`), or a systemd unit with `WorkingDirectory=` set to the
repo; `SIGINT`/`SIGTERM` trigger a graceful shutdown (flush market cache,
`store.close()`).

Only one process may own a data directory, and only one socket may use a given
Rollercoin session — two of either will duplicate orders.

## 3. Module map

> Historical porting record. The single-file modules below have since been split
> into directories (`src/engine/`, `src/store/`, `src/server/`, `src/api/`,
> `src/backup/`, `src/runtime/`, …) with one responsibility per file. The
> current layout is documented in `README.md` under **Development**.

| Origin (JS, removed) | Rust | Status |
| --- | --- | --- |
| `src/shared/constants.js` | `src/constants.rs` | ported |
| `src/shared/rules.js` | `src/rules.rs` | ported + tests |
| `src/shared/depth.js` | `src/depth.rs` | ported + tests |
| `src/shared/timings.js` | `src/timings.rs` | ported + tests |
| `src/shared/raw-events.js` | `src/raw_events.rs` | ported + tests |
| `src/shared/redact.js` | `src/redact.rs` | ported + tests |
| `src/shared/validation.js` | `src/validation.rs` | ported + tests |
| `src/worker/config.js` | `src/config.rs` | ported + tests |
| `src/worker/logger.js` | `src/logger.rs` | ported + tests |
| `src/worker/db.js` + `schema.js` | `src/db.rs` | ported + tests |
| `src/worker/store.js` | `src/store.rs` | ported (core) + tests |
| `src/worker/server.js` (HTTP/WS) | `src/server.rs` | ported — see §4 |
| `src/shared/trend.js` | `src/trend.rs` | ported + parity fixtures + benchmark |
| `src/worker/events.js` | `src/events.rs` | ported + tests |
| `src/worker/engine.js` | `src/engine.rs` | ported (event-driven) + tests |
| `src/worker/index.js` (wiring) | `src/runtime.rs` | ported + end-to-end test |
| `src/worker/gateway.js` | `src/gateway.rs` | ported (native-tls) + tests |
| `src/worker/latency.js` | `src/latency.rs` | ported + tests |
| `src/worker/api.js` + `http.js` | `src/api.rs` | ported (rustls + HTTP/2) + tests |
| `src/worker/session.js` | `src/session.rs` | helpers ported + tests (scheduler in wiring) |
| `src/worker/keepalive.js` | `src/keepalive.rs` | ported + tests |
| `src/worker/backup.js` | `src/backup.rs` | ported + tests (JSON v1 + `VACUUM INTO`) |
| `src/worker/system.js` | `src/system.rs` | ported (procfs) + tests |
| `src/worker/db-metrics.js` | `src/db_metrics.rs` | ported (`trace_v2`) + tests |
| `src/worker/item-info.js` | `src/item_info.rs` | ported + tests |
| `src/worker/mock.js` | `src/mock.rs` | ported + tests (local fills, no upstream) |
| `ui/`, `dist/` | unchanged | React deck is served as-is |

## 4. HTTP + WebSocket parity

| Route | Rust | Notes |
| --- | --- | --- |
| `GET /api/health` | ✅ | unauthenticated, same shape |
| `GET /api/state` | ✅ | full snapshot: gateway, session, latency, keepalive, system, configs, pending |
| `GET /api/configs` | ✅ | `runtime` is filled by the engine once a target has seen a frame or a cached snapshot |
| `PUT /api/configs` | ✅ | same 422 `fields` map, bulk enrichment runs after the reply |
| `POST /api/configs` | ✅ | 201 + label auto-enriched before the reply (1.5 s budget) |
| `PATCH /api/configs/:id` | ✅ | merge semantics match `{...existing, ...body}`; enrichment follows async |
| `DELETE /api/configs/:id` | ✅ | 404 when unknown |
| `PATCH /api/settings` | ✅ | validated ranges, broadcasts `settings` |
| `POST /api/control` | ✅ | `off`/`shadow`/`live` (legacy `pause`/`resume`/`toggle` aliases) and `reconnect` |
| `GET /api/history` | ✅ | cursor + offset + `q`/`status` filters; rows omit the two JSON payloads |
| `GET /api/history/:id` | ➕ | Rust-only: `timings` + `rawEvents` for one row (the detail modal) |
| `DELETE /api/history` | ✅ | wipes the loot log + profit report and resets stats, engine counters and latency samples |
| `GET /api/report` | ✅ | same derived columns |
| `GET /api/events` | ➕ | Rust-only: marketplace-command counts (listing/price updates and fulfillment results) per 5-minute bucket for `?days=1..7`; the deck sums them into the chart timeframe in local time |
| `GET /api/item-activity` | ➕ | Rust-only: market-frame counts **per item id** in 5-minute buckets over `?days=1..30` or an explicit `?since=&until=`. Takes `itemType` and `q` filters plus `limit` (default 25, max 250) and `offset`; answers a ranked page with `events`, `events24h` and a `tracked` fleet flag, alongside range-wide `totalRows`, `totalEvents` and `typeTotals` computed before filtering and paging. Ids never added to the target fleet are included; names come from the `item_catalog` cache, and a bounded, rotating batch of unresolved ids is looked up through `item-info` on each response |
| `GET /api/price-history` | ➕ | Rust-only: one duration-weighted median close per minute plus its held and raw low/high bands for a single `itemId` over `?days=1..7`; the deck buckets them in local time and merges the live 24h window |
| `GET /api/session` | ✅ | same preview/expires fields |
| `POST /api/session` | ✅ | validated, `changed` flag |
| `DELETE /api/session` | ✅ | clears all three credentials |
| `POST /api/auth/rotate` | ✅ | 409 when `WORKER_TOKEN` pins the key |
| `GET/POST /api/backup` | ✅ | JSON v1 + SQLite snapshot; 64 MB body limit; safety snapshot on import |
| `GET /api/latency` | ✅ | four-channel snapshot |
| `POST /api/latency/probe` | ✅ | on-demand warm probe; also broadcasts `latency` |
| `GET /api/system` | ✅ | live sampler (CPU/RAM/load/loop lag/db files) |
| `GET /api/benchmark` | ➕ | Rust-only: per-stage order pipeline stats (avg/p50/p95/jitter) over the last 25/50/200 timed orders, rebuilt from `orders.timings` |
| `GET /api/benchmark/history` | ➕ | Rust-only: local-clock buckets (`?hours=1..168&bucket=5m..1d&stage=total&tzOffsetMs=`) with per-bucket avg/p95/jitter plus a range summary, for the expanded latency chart |
| `GET /api/item-info` | ✅ | resolver with type fallbacks; updates the target label/rarity |
| `POST /api/session/push`, `/refresh` | ✅ | bridge merge rules + coalesced refresh |
| `WS /ws` (`rcw.v1`) | ✅ | token as 2nd subprotocol or `?token=`; `hello`, `snapshot`, `ping`/`pong`, `refresh`, broadcasts |

Static files: `dist/` is served with an SPA fallback to `index.html`, same as the
Bun worker.

## 5. Deliberate differences (permanent)

1. **No legacy JSON import.** `state.json` / `loot-log.json` were never ported;
   the backend targets the SQLite v8 schema directly. v7 files are migrated in
   place: `settings.mode` is added and backfilled from `paused` / `dry_run`.
2. **`save()` flushes immediately.** The Bun worker debounces 120 ms. A flush
   measures ~40 µs here, so synchronous writes stay until the frame path has a
   measured reason to defer them.
3. **`runtime` is `null` until a target has a live frame or a cached snapshot,**
   and `pending` is `[]` until an order is in flight. `latency`, `benchmark`,
   `system` and `keepalive` are always populated. The deck handles the nullables
   with optional chaining (verified in the browser).
4. **No auth-failure rate limit yet.** The Bun worker returns 429 after 20 bad
   keys per IP per minute.
5. **Logger timestamps are UTC `HH:MM:SS`;** the JS one uses local time.
6. **`MOCK=1` seeds demo targets and a synthetic session** when the store is
   empty, then simulates the feed and purchases locally (92% fill, 8% fail,
   500–1400 ms). The warm probe and the session tick are disabled in this mode.
7. **The engine is event-driven.** `handle_frame` returns `Broadcast`/`Submit`
   events instead of performing I/O itself; the wiring owns the store lock and
   spawns the purchase call, then feeds `PurchaseOutcome` back. Behaviour (gates,
   cooldown, rate cap, backoff, pending matching, duplicate suppression, early
   result replay) is covered by unit tests with a fake clock.
8. **The 401 → refresh → retry loop and the 30 s session tick live in
   `runtime.rs`**, not in `ApiClient`: the client reports `unauthorized`, the
   wiring refreshes (coalesced) and retries once.
9. **The warm probe's first run waits one interval** (30 s default) instead of
   firing at boot, matching `#schedule(intervalMs)`; `wake()` preempts the wait
   after a session change, target edit, or trading resume.
10. **System metrics come from procfs, not V8.** `memory.heap*` and
    `eventLoop.utilization` are `null`; loop lag is measured by a 100 ms
    watchdog task (scheduler delay) instead of a heap histogram.
    DB statement metrics are real: SQLite's `trace_v2` profile callback feeds
    `db_metrics.rs` without wrapping a single call site. Failed prepares and
    `SQLITE_BUSY` are not visible from profile events and stay zero unless the
    store reports them via `mark_error`/`mark_busy`.
11. **The warm probe shares the order client's pool.** Bun's global `fetch`
    pooled sockets per origin, so the old probe kept the purchase connection
    hot for free. A `reqwest::Client` owns its pool, so `KeepaliveConfig` takes
    `ApiClient::http_client()`; a probe with its own client would warm a
    second, unused socket and orders would still pay TCP + TLS after an idle
    gap. `tests/connection_pool.rs` locks this in by counting connections.

## 6. Verification during the port

- `cargo test`: **234 tests** (216 unit + 12 HTTP + 3 end-to-end + 3 pool). Rules, depth,
  timings, raw events, redaction, validation, config, logger, DB schema, store
  round-trips/paging/timings merge, server helpers, **trend parity fixtures**,
  events normalisation, thirty-nine engine scenarios, six API-client scenarios against
  a mock upstream, the session freshness rules plus five bridge-merge cases,
  six latency-tracker cases, five keepalive cases (200, 429 suspend, transport
  backoff, real-traffic skip, disabled), eight system-monitor cases, four
  DB-metrics cases plus a real `trace_v2` duration test, six backup cases, five
  item-info cases, three simulator cases, and two gateway cases against a local
  WebSocket server.
- **Security headers parity**: every response carries `x-content-type-options`,
  `referrer-policy`, `x-frame-options` and `permissions-policy`; HTML documents
  additionally get the same CSP as the Bun worker. Verified in a browser: the
  deck renders under that CSP with an empty console.
- **The Bun suite passed untouched while both backends existed**: 171 tests, 0
  failures. It was deleted together with the JS worker; the remaining UI tests
  live in `ui/test/`.
- **HTTP surface** (`tests/http_api.rs`): health is public, `/api/state` is 401
  without the key, invalid targets return the 422 field map, and
  `POST /api/configs` resolves `"Rare Hashboard"` (and persists
  `rarityColor: a52bff`) through a mock item-info upstream within the 1.5 s
  enrichment budget.
- **Decision parity on real data**: `bun run parity` replayed 294 frames (7 raw
  `priceEvent` payloads captured from production orders + 287 price-point
  frames) through both engines. Every field matched — `hot`, trend
  state/level/proximity/band, and dry-run `qty`/`notional` — for all 294 frames,
  including 43 buy decisions.
- **Simulator end to end** (`tests/wiring.rs::mock_mode_trades_without_any_upstream`):
  with `MOCK=1` and an unreachable API base, the seeded targets trade, fills
  land with `mock_*` order ids, and the snapshot reports
  `gateway.upstream = "mock"` with the warm probe disabled. A live run showed
  3 orders / 3 fills in 6 seconds, and the React deck rendered it end to end
  (`MOCK` badge, `ONLINE`, `MOCK TOKEN ACTIVE`, 282 market events).
- **Bridge session flow** against the running binary: a new token is accepted
  (`accepted: true`, `source: "bridge"`), the same token is not re-accepted (it
  only backfills a *missing* refresh token, matching the JS rule), and
  `GET /api/session` reflects the pushed credentials.
- **Backup round trip** against the running binary: `GET /api/backup` returned
  the versioned JSON with `Content-Disposition`, `?format=sqlite` returned a
  69 KB `application/vnd.sqlite3` file, and posting that JSON back restored the
  target list and wrote `backups/pre-import-….db`. A 400 KB body was accepted
  (422 validation) proving the route's 64 MB limit overrides the global 256 KB
  one.
- **Live DB metrics** from the running binary after four API calls:
  `instrumented: true`, 14 statements (11 read / 3 write), window count 14,
  transaction counter active, history filling. A 200k-row recursive query in the
  test suite proves the profile callback reports real durations, not zeros.
- **Live system snapshot** from the running binary: 12 cores, process CPU 0.5%,
  host 12.2%, RSS 20.3 MB, system RAM 62.7% used,
  scheduler-lag p95 0.6 ms, SQLite 3.54.1 with real DB/WAL/SHM sizes, health
  `ok`.
- **End-to-end loop** (`tests/wiring.rs`): mock market feed + mock purchase API
  drive `runtime::Worker` for real. A frame below target produces a purchase,
  the accepted order id is matched, a fill lands in the loot log with stats
  (`filled=1`, `bought=10`, `spent=0.9`), and the snapshot reports
  `gateway.status = "online"` with both `http` and `wsHandshake` latency
  samples.
- **Market feed** is exercised end to end against an axum WS server: connect,
  receive a `marketplace_orders_update` frame, parse/count it, record the
  handshake latency, and reconnect on demand (`reconnects` reaches 2).
- **Upstream HTTP client** is tested against a local axum mock: bearer/cookie
  headers, `Origin`/`Referer`/UA, purchase payload (`totalCount`, `totalPrice`),
  `data.usersPurchaseOrder` parsing, refresh without the expired bearer,
  item-info query string, and the non-JSON/Cloudflare hint.
- **Trend numerical parity.** Three series (flat 6 h, linear decline 6 h, sine
  24 h) are evaluated by the JavaScript model and the expected numbers are
  pinned in `trend::tests::matches_the_javascript_model_on_recorded_series`.
  Trend, fast, momentum, slope, t-stat, noise, confidence, error band,
  proximity and state all match the JS output.
- **Trend benchmark** (`cargo run --release --example bench_trend`, 1 441 pts):

  | Implementation | p50 | p95 |
  | --- | ---: | ---: |
  | Bun/JS (recorded in the feasibility study) | 1 248 µs | 2 158 µs |
  | Rust port (allocation-free Hampel) | **161 µs** | **248 µs** |

  ~7.7× faster with the same maths. Further wins (reusing buffers across frames)
  are possible but not needed for the latency budget.
- `curl` against a live process: health, 401 gate, config CRUD with the 422
  field map, settings patch, history/report, pinned-token 409.
- Control socket via a Bun client: `hello` → `snapshot` → `pong` with a valid
  key; close code **4001** with a bad key.
- Realtime parity: the control socket owns the telemetry ticker. `gateway`,
  `system` and `latency` are pushed together once every 3 s
  (`TELEMETRY_INTERVAL_SECS`); pushing `gateway` per frame re-rendered the HUD
  continuously, so it was deliberately coalesced, and `MissedTickBehavior::Delay`
  stops a stalled socket from firing a catch-up burst. A manual session change
  bumps `gateway.reconnects`, so the feed re-handshakes with the new token.
- Failed no-item orders keep the attempted `qty`/`unitPrice`/`targetPrice` and
  the full dispatch→result `latencyMs` in the loot row, matching the Bun
  `failureMeta = pendingMeta` path (covered by an engine test).
- The loot latency debugger measures **TTFB inside the HTTP client**: the
  `post_json` call records `sendAt` just before dispatch, `responseAt` the
  moment the first response byte arrives (for HTTP, the response head, before
  the body is read), and a monotonic `ttfbMs` from the same boundary. TTFB is
  surfaced twice: a dedicated **TTFB** tile shows the monotonic per-attempt
  value, and the timeline's "TTFB · Time To First Byte" stage shows it on the
  wall-clock timeline. Body read + JSON parse are charged to the next stage.
  `ttfbMs` is per attempt: on a 401 refresh + retry the first dispatch still
  anchors the span (so the refresh is visible) while the retry supplies the
  stored TTFB. A transport error records no TTFB rather than a fast 0 ms.
  The fulfillment wait is anchored to that TTFB mark (`sendAt + ttfbMs`), not
  to the completed response: body parsing, acceptance and marketplace matching
  are all inside the wait. Retry and legacy rows without a usable TTFB sample
  fall back to `responseAt`. Mock runs have no HTTP exchange, so they fall back
  to the completion mark.
- Connection classification still uses the recording resolver on the shared
  `reqwest` client: hyper only resolves a name when it opens a connection, so
  `socketMode = "new"` means this purchase paid DNS + TCP + TLS, while
  `"reused"` reused a pooled keep-alive socket. IP-literal upstreams bypass the
  resolver (`skipped` / `unknown`). The DNS numbers themselves are no longer a
  UI tile — they were almost always 0 behind the warm probe and duplicated the
  Connection tile — but the `dns*` fields stay in `orders.timings` as debug
  evidence.
- Attribution is **per request, not per client**. `DnsRecorder::begin` opens a
  window and the HTTP call is polled inside a task-local request id; the
  resolver only records into the window of the task that is actually
  connecting. Concurrent purchases, the warm probe and label enrichment
  therefore cannot leak a lookup into another order. hyper-util polls the
  connector in the request task (it does not spawn), which is what makes the
  task-local approach exact. Windows are dropped with their guard, so cancelled
  calls do not accumulate.
- Three cases deliberately answer `unknown` instead of guessing: a proxied
  client (the resolver measures the proxy hop, not the upstream), an exchange
  that produced no response (a timeout is not "reused"), and more than one
  connection attempt in the same window (the socket is still `new`, but the
  ambiguous sample is dropped). Lookups outside any window are ignored.
  `tests/connection_pool.rs` covers the new→reused sequence against
  `localhost`, a foreign lookup that must not flip reuse into new, and the
  shared-pool warm probe; `tests/wiring.rs` covers the skipped/unknown branch
  against `127.0.0.1`.
- React deck opened in a real browser against the Rust worker: login with the
  access key, target list rendered, `PAUSED` state from the DB, no console
  errors. Screenshot: `.qa/rust-ui.png` (local, gitignored).
- Restart persistence: target row, `mode`, and `delayPerOrder` survive a
  process restart.
- `src/lib.rs` exposes the modules so `examples/` and future integration tests
  drive the exact code the binary ships.

## 7. Decision parity harness (removed with the JS engine)

Before the JS backend was deleted, `bun run parity` built a dataset from the live
database, replayed it through both engines, and diffed every field. The scripts
(`scripts/parity-*.mjs`) went away with the JS engine; the evidence stays here
and in git history.

**Result on the production database: 294 frames identical — 43 hot frames and 43
dry-run orders agree exactly**, including trend state and notional. The dataset
mixed every `orders.raw_events.priceEvent` (real upstream frames that produced an
order) with one synthetic frame per stored `price_points` row for active targets.

Current regression coverage for the same behaviour lives in the Rust tests:
trend fixtures (`src/trend/`) pin the model's numbers, and the engine suite
covers gates, dry-run quantities, pending fills and duplicate suppression.
`examples/replay.rs` can still replay a JSON frame list through the engine if you
keep one around.

## 8. Frame cost (Rust vs Bun)

`cargo run --release --example bench_frame` measures `engine.handle_frame` with
a real SQLite store and a full 24 h history (1 440 points), in two regimes:

| Regime | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| Rust burst (100 ms/frame, caches warm) | 17.2 µs | 29.4 µs | 185 µs |
| Rust steady (1 s/frame, snapshot writes + rebuilds) | 27.8 µs | 185 µs | 194 µs |
| Bun hot path, trend cached (recorded) | 12.2 µs | — | 27.4 µs |
| Bun trend rebuild (recorded) | 1 248 µs | 2 158 µs | — |
| Rust trend rebuild | 161 µs | 248 µs | — |

Reading it honestly:

- the **median tick** is ~5 µs slower in Rust than the Bun number — both are
  noise next to a 350 ms upstream round-trip, and the Rust path was written for
  clarity, not micro-tuning;
- the **worst-case frame is 7.7× cheaper** (161 µs vs 1 248 µs), which is what
  the feasibility study identified as the real tail; the Rust p99 is a trend
  rebuild, not a GC pause;
- there is **no GC**, so the 4–7 ms JS outliers have no equivalent here.

The benchmark paid for itself twice: it exposed `band_changed = true` (every
frame wrote a snapshot) and showed that uncached `prepare` dominated writes.
After fixing both (`band_changed` derived from the frame, `prepare_cached` on the
hot statements) the burst p50 dropped from 44.8 µs to 17.2 µs.

## 9. Next milestones

The backend is feature-complete and the only backend; what remains is optional
hardening rather than parity work:

1. **Optional store tuning** — cache the remaining statements (`orders`,
   `targets`) if the sustained p50 matters; today the frame path is well inside
   the budget.
2. **Auth-failure rate limit** — the JS worker returned 429 after 20 bad keys per
   IP per minute; the Rust server has no such limiter yet.
3. **Recalibration tool** — the trend calibration factor is a constant; a Rust
   replay tool could recompute it from `price_points` instead of editing code.

## 10. TLS stack and fingerprint risk

The old `ws` client (via `node:tls`) could fail the TLS handshake to the
Rollercoin edge while Bun's native stack succeeded. The Rust worker therefore
proved connectivity first:

```bash
bun run diagnose:ws      # cargo run --release --example diagnose_ws
```

Measured from this machine against the edge (the TCP/TLS rows use the
native-tls probe that also drives the gateway; the HTTPS row now runs through
the `reqwest`/rustls client):

| Step | Result |
| --- | --- |
| DNS | 3 A records in 52 ms |
| TCP connect | 67 ms |
| TLS handshake | 76 ms, cert 927 B DER |
| HTTPS GET `rollercoin.com` | 200 OK over HTTP/2 |
| WS upgrade without a token | **401 Unauthorized** (TLS + upgrade path works) |

The gateway (WebSocket) keeps **native-tls (OpenSSL)**, the stack that
completed the handshake above. The HTTP order client uses **rustls**, chosen
over native-tls for portability. `rustls` has a distinctive ClientHello, so
the fingerprint risk in `docs/rust-rewrite-feasibility.md` §5 is **accepted,
not eliminated**: the live probe reaches the edge and upgrades, but if
Cloudflare ever challenges it, switch `reqwest` back to `native-tls` or move to
BoringSSL bindings.

HTTP/2 is explicit, not inherited. Keep the dependency as:

```toml
reqwest = { version = "0.13.5", default-features = false, features = ["json", "charset", "http2", "system-proxy", "rustls"] }
```

`default-features = false` keeps the TLS backend visible (reqwest 0.13 defaults
to rustls, but relying on that hid the backend once already). The `http2`
feature enables ALPN negotiation; `cargo run --example diagnose_ws` prints the
protocol (`HTTP/2.0`) next to the HTTPS status.

## 11. Why the port order matters

The feasibility study (`docs/rust-rewrite-feasibility.md`) found that the app
path is already ~1 ms and the real tail comes from the trend rebuild (1.2 ms per
target, 9.3 ms for eight) plus GC pauses. That is why `trend.rs` is milestone 1:
the Rust version has to fix the algorithm, not just change the language.

## 12. Repository state after the migration

- **Deleted:** `src/worker/`, `src/shared/` (the UI constants moved to
  `ui/src/lib/constants.js`), the worker tests in the old `test/` directory, the
  Bun-specific scripts (`dev.js`, `diagnose-ws.js`, `calibrate-trend.js`,
  `bench-hotpath.mjs`, `parity-*.mjs`, `trial-compare.mjs`), `trade.js` and
  `test.json`.
- **Promoted:** the crate now lives at the repo root (`Cargo.toml`, `src/`,
  `examples/`, `tests/`, `target/`).
- **New commands:** `bun start` / `bun run mock` (build deck + release binary),
  `bun run dev` (debug binary + Vite), `bun run test` (`cargo test && bun test`),
  `bun run key:reset` (Rust `reset_token` binary), `bun run diagnose:ws`,
  `bun run bench:frame`, `bun run bench:trend`.
- **Default data directory** is `./data`; the old side-by-side `./data-rs` was
  removed. Existing SQLite v7 files keep working: they are upgraded to v8 in
  place on first open, with `mode` backfilled from the old flags.
- **PM2** runs `target/release/rollercoin-worker` directly
  (`interpreter: "none"`); no Bun/Node runtime is involved in production.

---

## 13. Optimisation pass

Measured with `cargo run --release --example bench_frame`, same machine, full
24 h history (the benchmark warms that up first):

| Frame path | p50 burst | p50 steady |
| --- | ---: | ---: |
| Before | 18.4 µs | 28.7 µs |
| After | **5.0 µs** | **13.3 µs** |

What changed:

- **The market tick no longer builds a JSON tree per frame.** The trend model is
  serialised once per rebuild (every 10 s) and embedded as a `RawValue`; the tick
  itself is a typed struct. Re-serialising the flattened `TrendSignal` per frame
  measured at ~59% of the frame budget.
- **A headless worker builds no payload at all.** `process_frame` tells the
  engine whether a control socket is connected, and nothing is built when none
  is.
- **Market ticks are coalesced per item** before they reach the sockets
  (`MARKET_BROADCAST_INTERVAL_MS`, 250 ms). Only the newest tick per target is
  kept, so a busy book cannot flood the deck with hundreds of frames per second.
- **Broadcasts are serialised once** into a shared `Bytes` envelope, instead of a
  deep `Value` clone plus a `to_string` per receiver per event.
- **`normalize_upstream_event` borrows the `data` object.** Cloning the listing
  ladder cost 19.5 µs per frame on a 40-level book; borrowing costs 2.1 µs.
- **`estimate_fill_price_sorted`** consumes the ladder the normaliser already
  sorted instead of copying and sorting it a second time.
- **`Store::find_config` is an index lookup** (lowercase id → position, with a
  case-insensitive fallback). The old scan allocated a lowercase string per
  configured target on every lookup.
- **`flush_stats()`** persists only the counters an order changes; `flush()` used
  to rewrite every target row on each order.

Control plane and durability:

- A socket that lags the broadcast channel receives `{"type":"resync"}` and
  refreshes over HTTP. It no longer makes the server rebuild a full snapshot
  (every target's 24 h history) for one slow client.
- Backup, restore, the hourly price prune, the WAL checkpoint and the orphan
  sweep run through `blocking::run`, which uses `block_in_place` on the
  multi-threaded runtime so `VACUUM INTO` and large `DELETE`s do not park a
  reactor thread. Current-thread runtimes (tests) run the closure inline.
- The data directory is locked with an advisory file lock (`.worker.lock`). A
  second worker now fails at boot with an explicit message instead of trading
  against the same database (and duplicating orders).
- `gateway.dropped` counts frames the core loop could not take because its queue
  was full. Market frames may be dropped (the next tick supersedes them); fill
  frames use `send().await` and are never dropped.
- Store writes that used to be silently ignored (target upserts, session
  persistence, the order counter) now log at most once a minute. A full disk
  used to look exactly like a healthy worker.
- Loot lists (boot hydration, `/api/history`, `/api/state`) no longer select
  `timings`/`raw_events`. SQLite's profile timer runs until the statement
  resets, so decoding the two JSON columns while iterating rows was reported as
  a slow `SELECT` even when the query itself was microseconds. `GET
  /api/history/:id` loads them for the single row a detail modal opens.
- `orders (at DESC, id DESC)` keeps the loot page sort-free, and
  `price_points (bucket_at)` lets the hourly retention `DELETE` seek instead of
  scanning every retained row.

Price history:

- **The stored close is a duration-weighted median, not the last frame.** The
  feed is change-driven and `listings[0]` can be a one-unit bait ask, so taking
  the minute's last write as its close let a single transient print redraw the
  chart and then propagate into every coarser bucket. `MinuteAgg` weighs each
  price by how long it held; a print seen for less than half the minute cannot
  move the close, while a sustained move still wins. A
  `PRICE_SETTLE_DWELL_MS` (3 s) debounce runs first, so a price that vanishes
  within a few seconds never enters the weights. The guard only shapes the
  chart and sampler — order decisions still read the raw best ask, so a real
  cheap listing is never delayed.
- **The minute is finalised when it rolls.** The frame path and the quiet-minute
  sampler both close the previous bucket with the exact median, including the
  tail after the last frame, and carry the settled level into the new minute.
- **Every bucket also carries a held and a raw band.** `MinuteAgg` widens
  `low`/`high` over the prices that actually held (post-dwell) and
  `raw_low`/`raw_high` over every print, so the chart can show either. They are
  stored in `price_points.low_price`/`high_price` and
  `raw_low_price`/`raw_high_price`; rows from an older database have NULL edges
  and are read as `low = high = price`, so the chart draws a zero-width band
  instead of a hole without paying for a mass UPDATE at startup.
  `/api/price-history` returns `l`/`h` and `rl`/`rh` next to `p`. The expanded
  chart draws the selected band behind the close line, has a Held/Raw switch,
  and lists Held low/high and Raw low/high in its tooltip.

Still open (deliberately unchanged): per-request auth rate limiting, loot-log
retention plus `COUNT(*)` caching for a very large `orders` table, and a release
profile with LTO.

## 14. Early rearm and local reconciliation

The feed is the slow part of a trade, so waiting for it twice is expensive: an
order leaves stock behind, and the old engine only bought that leftover when the
*next* `marketplace_orders_update` frame re-advertised it — one extra WS hop
after the fill.

**Early rearm.** `handle_fill` re-plans the level itself (`rearm_after_fill`)
instead of waiting for that frame. The quantity no longer comes from the intent's
unfilled remainder (`qty - got`); it comes from the same `plan_order` call a frame
would make, against the units the level still holds, so every quantity
relationship is covered:

- `got < sent` — a partial fill leaves the rest of the request on the book;
- `sent < ready` — the request never covered the best level, so `ready - sent`
  units are still listed and would otherwise wait one WS hop;
- a full fill of the whole level — the floor is 0, the planner returns `no-stock`
  and nothing is fired.

It goes through `plan_order` like any other order, so the price ceiling, the
minimum and `allowPartialOrder` still decide, and high-stock doubling is disabled
(a fill confirms what was taken; it is not a new signal). `place_order` was
extracted from `handle_market` so the follow-up cannot skip a gate: it only opts
out of `delayPerOrder` (it extends a fill the engine has just booked) and
still pays a rate slot. A rejected follow-up is recorded but does not trip the
backoff, because a wrong guess about the leftover is an expected outcome and the
backoff would freeze the target for far longer than the frame wait it replaced.

**Reconciliation.** Frames carry no `updatedAt` and no sequence number (verified
against the raw frames in `orders.raw_events`), so a snapshot generated before
our purchase was applied can arrive after it and still show the pre-purchase
depth. Trusting it would resurrect stock that is already gone. `Runtime::reconcile`
is therefore a local ledger of the best level for `RECONCILE_WINDOW_MS` (1.5 s):

- it opens when the order is **submitted**, reserving the whole request out of
  the level, so neither a stale frame nor a second order can spend the same units
  in the gap between the HTTP response and the result frame;
- a fill returns only the part upstream did not take (`sent - got`) and records
  what it did take; a result that obtained nothing returns the whole request;
- a frame may lower the decision stock but never raise it above the floor;
- the window closes early — and the frame wins — when its best price moved to a
  different level, when it shows a *fuller* best level than the one that was
  bought from (a pre-purchase snapshot cannot contain new listings), when it
  shows at most the floor, or when its `totalCount` dropped by at least what was
  bought (the total proof only applies once a fill has been booked, otherwise
  "dropped by zero" would close the window on the first frame);
- on expiry the frame wins again, so a genuine restock is invisible for at most
  the window;
- a result that arrives after the window closed is booked but never reopens it:
  the feed has been authoritative for longer than the window, so rebuilding a
  floor from the old trigger could only resurrect stock that is gone.

Settling against the ledger instead of recomputing `stockAtTrigger - got` at
every fill is what stops the early update from firing a wrong buy when the real
result lands: any order placed in the meantime already spent its own share of
the floor, and recomputing would hand those units to the fill as well — the
double-buy the early rearm used to trigger.

The price and depth proofs matter: without them a full fill of a whole level
(floor 0) would blind the engine to a genuinely cheaper listing that appears on
the very next frame. Only a frame at the *same* level and the *same* depth as the
pre-purchase book is indistinguishable from a stale one, and that case waits out
the window.

The two concepts are kept apart on purpose: `Runtime::last_stock` stays the raw
upstream number and feeds the snapshot and the trend's book size, while the
decision path uses `effective_stock`. Each market tick carries both (`stock`,
`effectiveStock`) plus a `reconciling` flag, and `runtime.reconcile` is exposed
in the runtime snapshot for debugging. Nothing about the window is persisted: it
is a within-process view that a restart simply drops.

Covered by `engine::tests::a_partial_fill_rearms_the_remainder_without_waiting_for_a_frame`,
`a_full_fill_with_stock_left_rearms_early`,
`a_submitted_order_protects_its_units_before_the_fill_lands`,
`the_early_update_never_spends_the_leftover_twice`,
`a_late_fill_does_not_reopen_the_window`,
`a_failed_fill_gives_its_reservation_back`,
`an_over_fill_cannot_inflate_the_book`,
`a_stale_frame_cannot_resurrect_stock_a_fill_already_consumed`,
`a_frame_that_shows_at_most_the_floor_closes_the_window_early`,
`an_expired_window_lets_the_frame_win_again`,
`a_frame_whose_total_count_dropped_closes_the_window`,
`a_frame_fuller_than_the_book_we_bought_from_is_trusted`,
`a_moved_best_price_closes_the_window`,
`a_stale_frame_at_the_cleared_level_stays_clamped_after_a_full_fill`,
`a_fill_that_beats_the_http_response_still_rearms`,
`an_optimistic_follow_up_rejection_does_not_back_off_the_target`,
`a_follow_up_never_doubles_a_deep_book`,
`a_follow_up_respects_the_current_target_price`,
`a_remainder_below_the_minimum_is_not_chased`,
`off_mode_stops_the_local_rearm`, `a_follow_up_needs_a_linked_session` and
`a_duplicate_fill_cannot_rearm_twice`.

---

## 15. Item activity monitor

`GET /api/events` counts marketplace commands as a whole (listing/price
updates and fulfillment results); prediction-market, reward and exchange-rate
frames are excluded. `GET /api/item-activity` answers a
different question — *which item ids moved most* — which needs the count keyed
by item, so it gets its own tables:

| Table | Row | Retention |
| --- | --- | --- |
| `item_event_points` | one `item_id` × one 5-minute slice: `events`, `item_type`, `last_seen_at` | 31 days |
| `item_catalog` | one row per id ever seen: resolved `label`/`rarity`, `first_seen_at`, `last_seen_at`, `resolved_at`, `attempts` | none (grows with the catalogue) |

Both are additive: a build that predates them ignores the tables, so
`user_version` stays at 8 and `migrate()` only has to add the two `item_catalog`
columns the rotation needs.

Slices are **5 minutes**, not an hour. The monitor's finest window is one hour,
so hourly storage could not answer it without counting a partly-elapsed hour as
a whole one; 5 minutes divides the hour evenly (12 slices), matches
`EVENT_BUCKET_MS`, and keeps a month of a busy feed to a few thousand rows per
item. Note that widening or narrowing `ITEM_EVENT_BUCKET_MS` invalidates
existing rows, because `bucket_at` encodes the slice grid — change it only with a
migration.

### Count path

`Engine::handle_frame` counts the frame **before** the target lookup, so an id
that is not on the fleet is still ranked — that is the discovery half of the
feature. Only `RLT` frames are counted (a frame with no `currency` counts too,
matching the engine's own currency gate). Counting is an in-memory
`BTreeMap<(slice, item_id), _>` bump; `Worker` flushes dirty buckets in one
transaction every 15 s and on shutdown, exactly like the global event counter.

### Ranking and paging

`Store::item_activity` aggregates the in-memory map (the same window the flusher
persists, so nothing is double counted), builds the full ranking, sums the
range-wide totals, and only then slices the requested page. Order is `events`,
then `events24h`, then id. Per row it derives:

- `events` — frames in the requested range; `events24h` — frames in the last 24 h
  regardless of the range, so the heat column keeps meaning "busy right now"
  while the table is scoped to a narrow window;
- `firstAt`/`lastAt` — the newest and oldest frame the product produced inside
  the range;
- `tracked` — whether the id is currently on the target fleet.

A per-row "active time" (`activeHours`) and its derived `rate` used to be
returned here. They were dropped with the column that displayed them: counts are
stored in 5-minute slices, so a worker up for two minutes has all of its frames
in one slice, and any "per hour" figure derived from that describes uptime rather
than the market.

Paging happens after ranking, so the next batch continues the same order rather
than re-sorting a new subset. `totalRows`, `totalEvents` and `typeTotals` are
computed before the slice, which is what lets the header summary and the share
column stay still while the reader loads more rows. `typeTotals` is also summed
before the category and search filters: the deck keeps one badge per known type
on screen, so selecting a category must not make the other categories vanish
from the header or from the filter menu. `totalRows` and `totalEvents` still
describe the filtered table, and the deck renders a zero badge for known types
the range did not count. The deck uses the cursor style of the loot log — rows
accumulate and a **Load more** button asks for the next batch — so `offset` is
simply how many rows are already held.
`limit` defaults to `ITEM_ACTIVITY_PAGE_SIZE` (25) and caps at
`ITEM_ACTIVITY_LIMIT` (250); an `offset` past the end returns an empty page that
still reports the real `totalRows`.

The share column is `events / totalEvents`: a product's slice of every frame in
the range, so the column sums to about 100% and 180 of 2 176 reads 8%. An earlier
version drew a bar scaled to the busiest product next to that number, which let
one row claim a full track and "8%" at the same time; the bar was removed rather
than given a second denominator, and `totalEvents` is now the only quantity the
column needs.

`typeTotals` groups by the type each frame was counted under, not by whatever the
catalogue says later. `item-info` fills the catalogue in small batches, so
letting a late lookup rewrite the type would move rows between categories (and
change the summary) merely because a name arrived; `apply_catalog_item` therefore
keeps the first type it sees.

### Name resolution

`item-info` is the only source of names, and it is a network call, so the
response must not depend on it completing:

- `pending_names` (sync, under the store lock) picks up to 8 rows whose name is
  unknown and whose catalogue row is not already a cached miss, ordered by
  **fewest attempts first**, then by volume.
- `resolve_pending` (async, no lock held) asks `item-info` for those ids with a
  2.5 s budget per response.
- `apply_resolved` (sync, short lock) writes the outcomes: a success updates the
  catalogue and the in-memory row; a *definitive* miss (`not found` / `invalid`)
  is cached via `resolved_at` so it is never retried; a transport failure only
  bumps `attempts`, so a flaky network recovers later without starving other rows.

The attempt ordering is what makes the last rule safe. Without it a batch of 8
unnameable ids (a delisted product, or the simulator's random ids) would be
retried on every refresh and every other row would stay anonymous forever — the
bug this section exists to prevent. A target enriched by `POST /api/configs` also
updates its catalogue row, so a fleet item is named before its first frame.

Covered by `item_activity::tests::the_resolution_batch_rotates_instead_of_retrying_the_same_rows`,
`item_activity::tests::pagination_params_are_parsed_and_bounded`,
`store::tests::item_activity_ranks_every_id_by_frame_count`,
`store::tests::item_activity_filters_by_type_search_and_range`,
`store::tests::item_activity_pages_a_stable_ranking_with_range_wide_totals`,
`store::tests::item_activity_pages_within_a_type_filter`,
`store::tests::item_activity_summarizes_frames_per_type`,
`store::tests::item_activity_survives_a_reopen_and_updates_in_place`,
`store::tests::catalog_records_names_misses_and_boot_state`,
`store::tests::item_event_buckets_align_to_the_five_minute_grid`,
`engine::tests::counts_market_frames_for_ids_outside_the_target_fleet`,
`http_api::item_activity_ranks_ids_outside_the_fleet_and_names_them_from_item_info` and
`http_api::item_activity_paginates_without_changing_the_range_totals`.
