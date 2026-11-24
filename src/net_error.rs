//! Operator-readable descriptions for HTTP transport failures.
//!
//! `reqwest::Error`'s `Display` names only the step that failed, for example
//! `error sending request for url (https://rollercoin.com/api/marketplace/purchase-item)`.
//! The reason — a DNS lookup failure, a refused connection, a TLS alert or a
//! reset socket — lives in its `source()` chain, so logging `to_string()`
//! produced warnings that could not be diagnosed from the terminal.
//!
//! [`describe`] folds the failure class and the whole cause chain into one
//! redacted line, so a transport failure says what broke instead of only where.

use std::error::Error as StdError;

use crate::redact::redact_secrets;

/// Short operator-facing class, so a log line can be recognised before the
/// details are read. Timeouts win over `is_connect`: a connect timeout is both,
/// and "timeout" is the more useful answer.
pub fn classify(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_dns() {
        "dns failure"
    } else if error.is_connect() {
        "connection failure"
    } else if error.is_body() {
        "response body failure"
    } else if error.is_decode() {
        "response decode failure"
    } else if error.is_redirect() {
        "redirect failure"
    } else if error.is_builder() {
        "request build failure"
    } else if error.is_status() {
        "http status failure"
    } else if error.is_upgrade() {
        "upgrade failure"
    } else {
        "request failure"
    }
}

/// The failure class plus every cause, secrets removed:
///
/// ```text
/// connection failure: error sending request for url (http://127.0.0.1:1/probe):
/// client error (Connect): tcp connect error: Connection refused (os error 111)
/// ```
pub fn describe(error: &reqwest::Error) -> String {
    let mut messages: Vec<String> = Vec::new();
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(source) = current {
        let message = source.to_string();
        // Layers sometimes repeat the same text; keep each cause once.
        if !message.is_empty() && !messages.contains(&message) {
            messages.push(message);
        }
        current = source.source();
    }
    let mut description = classify(error).to_string();
    for message in messages {
        description.push_str(": ");
        description.push_str(&message);
    }
    redact_secrets(&description)
}

/// [`describe`] with the caller's configured deadline appended, because
/// reqwest's own timeout text does not say which budget expired.
pub fn describe_with_timeout(error: &reqwest::Error, timeout: std::time::Duration) -> String {
    format!("{} after {}ms", describe(error), timeout.as_millis())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn describes_a_refused_connection_with_its_cause() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/probe")
            .send()
            .await
            .expect_err("port 1 must refuse the connection");

        let description = describe(&error);
        assert!(
            description.starts_with("connection failure: "),
            "expected a connection class, got: {description}"
        );
        assert!(
            description.contains("error sending request for url (http://127.0.0.1:1/probe)"),
            "the failing request must stay visible, got: {description}"
        );
        assert!(
            description.len() > error.to_string().len(),
            "the cause must be appended, got: {description}"
        );
        assert!(
            description.contains("127.0.0.1"),
            "the cause must name the peer, got: {description}"
        );
    }

    #[tokio::test]
    async fn classifies_a_timeout_and_keeps_the_budget() {
        // Accepts the connection but never answers, so the client deadline fires.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let _accepted = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let error = client
            .get(format!("http://{address}/probe"))
            .send()
            .await
            .expect_err("the server never answers");
        assert!(error.is_timeout());

        let description = describe_with_timeout(&error, Duration::from_millis(50));
        assert!(
            description.starts_with("timeout: "),
            "expected a timeout class, got: {description}"
        );
        assert!(
            description.ends_with("after 50ms"),
            "the budget must be visible, got: {description}"
        );

        hold.abort();
    }

    #[tokio::test]
    async fn redacts_credentials_that_reach_the_url() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/probe?token=live.session.value")
            .send()
            .await
            .expect_err("port 1 must refuse the connection");

        let description = describe(&error);
        assert!(
            description.contains("token=<redacted>"),
            "the token must be redacted, got: {description}"
        );
        assert!(
            !description.contains("live.session.value"),
            "the token must not leak, got: {description}"
        );
    }
}
