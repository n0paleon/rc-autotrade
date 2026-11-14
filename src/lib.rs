//! Turu.RC worker backend.
//!
//! The modules are exposed as a library so integration tests and the
//! `examples/` benchmarks can drive the same code the binary runs.

pub mod api;
pub mod backup;
pub mod benchmark;
pub mod blocking;
pub mod clock;
pub mod config;
pub mod constants;
pub mod db;
pub mod db_metrics;
pub mod depth;
pub mod dns_probe;
pub mod engine;
pub mod events;
pub mod feed;
pub mod gateway;
pub mod item_activity;
pub mod item_info;
pub mod keepalive;
pub mod latency;
pub mod lock;
pub mod logger;
pub mod mock;
pub mod model;
pub mod num;
pub mod raw_events;
pub mod redact;
pub mod rules;
pub mod runtime;
pub mod server;
pub mod session;
pub mod store;
pub mod system;
pub mod timings;
pub mod trend;
pub mod validation;
