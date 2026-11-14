//! One handle shape for both feed implementations.
//!
//! The real gateway and the `MOCK=1` simulator expose the same surface — stats
//! JSON, status, reconnect, stop — so the HTTP layer, the snapshot builder and
//! the control endpoint never branch on which one is running.

use serde_json::Value;

use crate::gateway::GatewayHandle;
use crate::mock::MockHandle;

#[derive(Clone)]
pub enum FeedHandle {
    Gateway(GatewayHandle),
    Mock(MockHandle),
}

impl FeedHandle {
    pub fn stats_json(&self) -> Value {
        match self {
            FeedHandle::Gateway(handle) => handle.stats_json(),
            FeedHandle::Mock(handle) => handle.stats_json(),
        }
    }

    pub fn status(&self) -> String {
        match self {
            FeedHandle::Gateway(handle) => handle.status(),
            FeedHandle::Mock(handle) => handle.status(),
        }
    }

    pub async fn reconnect(&self, reason: &str) {
        match self {
            FeedHandle::Gateway(handle) => handle.reconnect(reason).await,
            FeedHandle::Mock(handle) => handle.reconnect(reason).await,
        }
    }

    pub async fn stop(&self) {
        match self {
            FeedHandle::Gateway(handle) => handle.stop().await,
            FeedHandle::Mock(handle) => handle.stop().await,
        }
    }
}
