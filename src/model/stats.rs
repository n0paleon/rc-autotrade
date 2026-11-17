use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub since: i64,
    pub profit_rlt: f64,
    pub spent_rlt: f64,
    pub bought: i64,
    pub filled: i64,
    /// Filled orders with non-negative profit. Zero profit is a win. This is
    /// the numerator of the session winrate (the fill count stays in `filled`).
    #[serde(default)]
    pub wins: i64,
    pub failed: i64,
    pub orders_sent: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<i64>,
    #[serde(default)]
    pub refresh_count: i64,
    #[serde(default)]
    pub last_error: Option<String>,
}
