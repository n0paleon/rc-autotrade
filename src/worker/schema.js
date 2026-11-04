/**
 * SQLite schema for the worker runtime store.
 *
 * The database lives next to the old JSON files (`data/rollercoin.db`) and is
 * opened in WAL mode so readers never block the frame loop. Every statement is
 * idempotent: `openDatabase()` can run it on each boot and only `user_version`
 * decides whether a real migration is needed.
 */

export const SCHEMA_VERSION = 1;

export const SCHEMA_SQL = `
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session (
  id              INTEGER PRIMARY KEY CHECK (id = 1),
  access_token    TEXT NOT NULL DEFAULT '',
  refresh_token   TEXT NOT NULL DEFAULT '',
  cookie          TEXT NOT NULL DEFAULT '',
  source          TEXT,
  updated_at      INTEGER,
  last_refresh_at INTEGER,
  refresh_count   INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT
);

CREATE TABLE IF NOT EXISTS settings (
  id                    INTEGER PRIMARY KEY CHECK (id = 1),
  delay_per_order       INTEGER NOT NULL,
  max_orders_per_minute INTEGER NOT NULL,
  dry_run               INTEGER NOT NULL DEFAULT 0,
  auto_refresh          INTEGER NOT NULL DEFAULT 1,
  paused                INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS targets (
  item_id              TEXT PRIMARY KEY,
  label                TEXT NOT NULL DEFAULT '',
  item_type            TEXT NOT NULL DEFAULT 'mutation_component',
  rarity_color         TEXT,
  target_price         REAL NOT NULL,
  desired_qty          INTEGER NOT NULL,
  high_stock_threshold INTEGER NOT NULL,
  min_acceptable_qty   INTEGER NOT NULL,
  allow_partial_order  INTEGER NOT NULL DEFAULT 1,
  active               INTEGER NOT NULL DEFAULT 1,
  created_at           INTEGER,
  updated_at           INTEGER
);

CREATE TABLE IF NOT EXISTS stats (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  since       INTEGER NOT NULL,
  profit_rlt  REAL NOT NULL DEFAULT 0,
  spent_rlt   REAL NOT NULL DEFAULT 0,
  bought      INTEGER NOT NULL DEFAULT 0,
  filled      INTEGER NOT NULL DEFAULT 0,
  failed      INTEGER NOT NULL DEFAULT 0,
  orders_sent INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS orders (
  id           TEXT PRIMARY KEY,
  order_id     TEXT,
  item_id      TEXT NOT NULL DEFAULT '',
  label        TEXT NOT NULL DEFAULT '',
  qty          INTEGER NOT NULL DEFAULT 0,
  unit_price   REAL NOT NULL DEFAULT 0,
  total_price  REAL NOT NULL DEFAULT 0,
  target_price REAL NOT NULL DEFAULT 0,
  profit_rlt   REAL NOT NULL DEFAULT 0,
  status       TEXT NOT NULL,
  at           INTEGER NOT NULL,
  latency_ms   INTEGER,
  note         TEXT,
  message      TEXT
);

CREATE INDEX IF NOT EXISTS orders_at ON orders (at DESC);
CREATE INDEX IF NOT EXISTS orders_item_at ON orders (item_id, at DESC);

CREATE TABLE IF NOT EXISTS report_products (
  product_key TEXT PRIMARY KEY,
  item_id     TEXT NOT NULL DEFAULT '',
  label       TEXT NOT NULL DEFAULT '',
  fills       INTEGER NOT NULL DEFAULT 0,
  failed      INTEGER NOT NULL DEFAULT 0,
  qty         INTEGER NOT NULL DEFAULT 0,
  spent_rlt   REAL NOT NULL DEFAULT 0,
  profit_rlt  REAL NOT NULL DEFAULT 0,
  first_at    INTEGER NOT NULL,
  last_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS market_snapshot (
  item_id           TEXT PRIMARY KEY REFERENCES targets (item_id) ON DELETE CASCADE,
  last_price        REAL,
  last_stock        INTEGER,
  allowed_price_min REAL,
  allowed_price_max REAL,
  allowed_at        INTEGER,
  item_type         TEXT,
  seen_at           INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS price_points (
  item_id   TEXT NOT NULL,
  bucket_at INTEGER NOT NULL,
  price     REAL NOT NULL,
  PRIMARY KEY (item_id, bucket_at)
) WITHOUT ROWID;
`;
