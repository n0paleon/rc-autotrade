use serde_json::Value;

use super::client::ApiClient;
use super::parse::message_from;
use super::types::{Credentials, HttpMarks, PurchaseRequest, PurchaseResult};

pub(super) fn round_duration_ms(duration: std::time::Duration) -> f64 {
    let value = duration.as_secs_f64() * 1000.0;
    (value * 1_000.0).round() / 1_000.0
}

impl ApiClient {
    /// Purchase one listing batch.
    pub async fn purchase(
        &self,
        credentials: &Credentials,
        request: &PurchaseRequest,
    ) -> PurchaseResult {
        let payload = request.payload();

        let (status, body, text, transport_error, marks) = self
            .post_json(
                "/api/marketplace/purchase-item",
                &payload,
                credentials,
                true,
                self.timeout,
            )
            .await;
        let HttpMarks {
            send_at,
            response_at,
            ttfb_ms,
            socket,
            dns,
        } = marks;
        let send_at = (send_at > 0).then_some(send_at);
        let response_at = (response_at > 0).then_some(response_at);

        if let Some(error) = transport_error {
            return PurchaseResult {
                ok: false,
                status: 0,
                error: Some(error),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        let unauthorized = status == 401;
        if !matches!(status, 200..=299) {
            let message = message_from(body.as_ref(), &text, "Purchase was rejected.");
            return PurchaseResult {
                ok: false,
                status,
                body,
                error: Some(message),
                unauthorized,
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        if body.is_none() {
            return PurchaseResult {
                ok: false,
                status,
                error: Some(
                    "Upstream returned a non-JSON response (Cloudflare challenge or maintenance?)."
                        .to_string(),
                ),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        let body = body.unwrap();
        let order_id = body
            .pointer("/data/usersPurchaseOrder")
            .or_else(|| body.pointer("/data/orderId"))
            .or_else(|| body.get("usersPurchaseOrder"))
            .and_then(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            });

        if body.get("success").and_then(Value::as_bool) != Some(true) || order_id.is_none() {
            let error = body
                .get("message")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Purchase was not accepted by Rollercoin.")
                .to_string();
            return PurchaseResult {
                ok: false,
                status,
                body: Some(body),
                error: Some(error),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        PurchaseResult {
            ok: true,
            status,
            order_id,
            body: Some(body),
            error: None,
            unauthorized: false,
            send_at,
            response_at,
            ttfb_ms,
            socket,
            dns,
        }
    }

    /// Classifies the connection one exchange used. The resolver window is
    /// per-request, so any lookup in `snapshot` belongs to this call; more than
    /// one attempt is ambiguous and drops the sample instead of guessing which
    /// lookup won. A missing response is never reported as a reused socket.
    pub(super) fn classify_connection(
        &self,
        snapshot: &crate::dns_probe::DnsSnapshot,
        response_received: bool,
    ) -> (
        crate::dns_probe::SocketMode,
        Option<crate::dns_probe::DnsLookup>,
    ) {
        use crate::dns_probe::{SocketMode, is_ip};

        if self.proxy {
            // The resolver measured a proxy hop, not the upstream.
            return (SocketMode::Unknown, None);
        }
        match crate::dns_probe::host_of(&self.base_url) {
            Some(host) if is_ip(&host) => (SocketMode::Unknown, None),
            // Never claim a pooled socket for an exchange that produced no
            // response; keep a failed lookup so the resolver error survives.
            Some(_) if !response_received => (SocketMode::Unknown, snapshot.last_completed()),
            Some(_) if snapshot.attempted == 0 => (SocketMode::Reused, None),
            Some(_) if snapshot.attempted == 1 => (SocketMode::New, snapshot.single_sample()),
            // More than one connection attempt: definitely a fresh connection,
            // but which lookup to attribute is ambiguous.
            Some(_) => (SocketMode::New, None),
            None => (SocketMode::Unknown, None),
        }
    }
}
