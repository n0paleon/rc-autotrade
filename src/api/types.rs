use serde_json::{Value, json};

/// The credentials a call needs. Borrowed so the caller keeps ownership.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub cookie: String,
}

#[derive(Debug, Clone, Default)]
pub struct PurchaseRequest {
    pub item_id: String,
    pub item_type: String,
    pub qty: i64,
    pub unit_price_raw: f64,
    /// Headroom added to `totalPrice` so the marketplace can fill the whole
    /// request instead of a partial slice. Validated as `totalPriceMarkupPct`
    /// in the worker settings; 0 keeps the exact listing total.
    pub total_price_markup_pct: f64,
}

impl PurchaseRequest {
    /// The listing total before headroom, in raw micro-RLT.
    pub fn base_total_price(&self) -> f64 {
        self.unit_price_raw * self.qty as f64
    }

    /// The `totalPrice` actually posted: the listing total plus the configured
    /// fulfillment headroom.
    pub fn total_price(&self) -> f64 {
        crate::rules::apply_total_price_markup(self.base_total_price(), self.total_price_markup_pct)
    }

    /// The exact JSON body posted to `/api/marketplace/purchase-item`.
    ///
    /// Kept in one place so the loot log can store what was actually sent
    /// without drifting from the request builder.
    pub fn payload(&self) -> Value {
        let item_type = if self.item_type.is_empty() {
            "mutation_component"
        } else {
            &self.item_type
        };
        json!({
            "challenge": "",
            "action": "marketplace",
            "itemId": self.item_id,
            "itemType": item_type,
            "totalCount": self.qty,
            "currency": "RLT",
            "totalPrice": self.total_price(),
        })
    }
}

/// Timing and connection marks for one HTTP exchange, taken inside the client
/// so the debugger can separate dispatch, first byte and body read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpMarks {
    /// Epoch ms just before the request was dispatched.
    pub send_at: i64,
    /// Epoch ms when the first response byte arrived; `0` when none did.
    pub response_at: i64,
    /// Monotonic dispatch → first response byte (TTFB). `None` when no
    /// response arrived, so a transport error never looks like a fast TTFB.
    pub ttfb_ms: Option<f64>,
    /// Whether this exchange opened a connection or reused a pooled one. It is
    /// classified at the send boundary, before the body is read.
    pub socket: crate::dns_probe::SocketMode,
    /// The resolver sample this exchange paid for, when it is unambiguous.
    pub dns: Option<crate::dns_probe::DnsLookup>,
}

#[derive(Debug, Clone, Default)]
pub struct PurchaseResult {
    pub ok: bool,
    pub status: u16,
    pub order_id: Option<String>,
    pub body: Option<Value>,
    pub error: Option<String>,
    pub unauthorized: bool,
    /// Epoch ms when the request actually left the client.
    pub send_at: Option<i64>,
    /// Epoch ms when the first response byte arrived, before the body was read.
    pub response_at: Option<i64>,
    /// Monotonic dispatch → first response byte (TTFB) for this attempt.
    pub ttfb_ms: Option<f64>,
    /// Whether the HTTP call opened a fresh connection or reused a pooled one.
    pub socket: crate::dns_probe::SocketMode,
    /// The resolver work this call actually paid for. `None` when the request
    /// reused a socket, the upstream is an IP literal, or no lookup completed.
    pub dns: Option<crate::dns_probe::DnsLookup>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshResult {
    pub ok: bool,
    pub status: u16,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ItemInfoResult {
    pub ok: bool,
    pub status: u16,
    pub data: Option<Value>,
    pub error: Option<String>,
}
