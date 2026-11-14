//! TLS uses rustls (same stack as the `reqwest` HTTP client in `api.rs`).

use std::sync::Arc;

use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::USER_AGENT;

pub(super) fn build_request(
    url: &str,
    token: &str,
    cookie: &str,
) -> Option<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut parsed = url::Url::parse(url).ok()?;
    if !token.is_empty() {
        parsed.query_pairs_mut().append_pair("token", token);
    }
    let mut request = parsed.to_string().into_client_request().ok()?;
    let headers = request.headers_mut();
    headers.insert("origin", "https://rollercoin.com".parse().ok()?);
    headers.insert("referer", "https://rollercoin.com/".parse().ok()?);
    headers.insert("user-agent", USER_AGENT.parse().ok()?);
    if !cookie.is_empty() {
        headers.insert("cookie", cookie.parse().ok()?);
    }
    Some(request)
}

pub(super) fn connector() -> Option<Connector> {
    ensure_crypto_provider();
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    let config = CONFIG
        .get_or_init(|| {
            let rustls_native_certs::CertificateResult { certs, .. } =
                rustls_native_certs::load_native_certs();
            let mut root_store = rustls::RootCertStore::empty();
            root_store.add_parsable_certificates(certs);
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            // WebSocket upgrade is HTTP/1.1; mirror the ALPN the previous
            // native-tls connector requested.
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone();
    Some(Connector::Rustls(config))
}

fn ensure_crypto_provider() {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}
