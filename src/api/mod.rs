//! Rollercoin HTTP client.
//!
//! The client is stateless: cookies and bearer tokens are passed in by the
//! caller (the session layer), so a token rotation only has to happen in one
//! place. Every call records `last_request_at`, which the warm probe uses to
//! decide whether a keepalive is still needed.
//!
//! Transport is `reqwest` with **rustls** and explicit **HTTP/2**. The Cargo
//! features are pinned with `default-features = false` so the TLS backend and
//! the `http2` flag stay visible; `examples/diagnose_ws` proves the stack
//! reaches the Rollercoin edge and negotiates `HTTP/2.0`. rustls has a
//! distinctive ClientHello, so that fingerprint risk is accepted rather than
//! eliminated.

mod client;
mod item_info;
mod parse;
mod purchase;
mod session;
mod types;

#[cfg(test)]
mod tests;

pub use crate::constants::USER_AGENT;
pub use client::ApiClient;
pub use types::{
    Credentials, HttpMarks, ItemInfoResult, PurchaseRequest, PurchaseResult, RefreshResult,
};

const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const ITEM_INFO_TIMEOUT_MS: u64 = 8_000;
const BODY_PREVIEW_CHARS: usize = 240;
