//! Rust port of the Turu.RC worker.
//!
//! The modules are exposed as a library so integration tests and the
//! `examples/` benchmarks can drive the same code the binary runs.

// The port is incremental: `rules`, `depth`, `timings`, `trend` and `redact`
// are fully tested but only the engine round calls some of them. Drop this
// allow once the engine, gateway and API modules are wired in.
#![allow(dead_code)]

pub mod api;
pub mod backup;
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
pub mod item_info;
pub mod keepalive;
pub mod latency;
pub mod logger;
pub mod mock;
pub mod model;
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
