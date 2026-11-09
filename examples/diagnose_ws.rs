//! Read-only upstream probe.
//!
//! Proves the rustls TLS stack can reach the Rollercoin edge *before* the
//! gateway work starts.
//!
//! Run: cargo run --release --example diagnose_ws
//!
//! It never sends an order, never prints a token, and only opens an
//! unauthenticated WebSocket (which the upstream is expected to reject).

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::RootCertStore;
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const WS_URL: &str = "wss://nws.rollercoin.com/";
const HTTP_URL: &str = "https://rollercoin.com/";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(12);

fn ok(label: &str, detail: impl AsRef<str>) {
    println!("  ok    {label:<28} {}", detail.as_ref());
}

fn fail(label: &str, detail: impl AsRef<str>) {
    println!("  FAIL  {label:<28} {}", detail.as_ref());
}

fn rustls_config() -> Arc<rustls::ClientConfig> {
    let rustls_native_certs::CertificateResult {
        certs, errors, ..
    } = rustls_native_certs::load_native_certs();
    if !errors.is_empty() {
        eprintln!("  warn  native certs load: {errors:?}");
    }
    let mut root_store = RootCertStore::empty();
    let (added, ignored) = root_store.add_parsable_certificates(certs);
    println!("  info  root certs: {added} added, {ignored} ignored");

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let ws = url::Url::parse(WS_URL).expect("valid ws url");
    let host = ws.host_str().unwrap_or("nws.rollercoin.com").to_string();
    let port = ws.port_or_known_default().unwrap_or(443);

    println!("upstream probe (rustls)");
    println!("  ws   {WS_URL}");
    println!("  http {HTTP_URL}");
    println!();

    // 1. DNS
    let started = Instant::now();
    match tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((host.as_str(), port))).await {
        Ok(Ok(addrs)) => {
            let addrs: Vec<_> = addrs.collect();
            ok(
                "dns",
                format!(
                    "{:?} in {:?}",
                    addrs
                        .iter()
                        .map(|addr| addr.ip().to_string())
                        .collect::<Vec<_>>(),
                    started.elapsed()
                ),
            );
        }
        Ok(Err(error)) => fail("dns", error.to_string()),
        Err(_) => fail("dns", "timeout"),
    }

    // 2. TCP
    let started = Instant::now();
    let tcp = tokio::time::timeout(TIMEOUT, TcpStream::connect((host.as_str(), port))).await;
    let tcp = match tcp {
        Ok(Ok(stream)) => {
            ok("tcp connect", format!("{:?}", started.elapsed()));
            Some(stream)
        }
        Ok(Err(error)) => {
            fail("tcp connect", error.to_string());
            None
        }
        Err(_) => {
            fail("tcp connect", "timeout");
            None
        }
    };

    // 3. TLS handshake (rustls, ALPN http/1.1 like the JS probe)
    if let Some(stream) = tcp {
        let config = rustls_config();
        let connector = tokio_rustls::TlsConnector::from(config);
        let started = Instant::now();
        let server_name = ServerName::try_from(host.as_str())
            .map(|name| name.to_owned())
            .expect("valid server name");
        match tokio::time::timeout(TIMEOUT, connector.connect(server_name, stream)).await {
            Ok(Ok(stream)) => {
                let negotiated = stream
                    .get_ref()
                    .1
                    .alpn_protocol()
                    .map(|alpn| String::from_utf8_lossy(alpn).to_string())
                    .unwrap_or_else(|| "none".into());
                let cert_len = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|certs| certs.iter().map(|cert| cert.len()).sum::<usize>())
                    .unwrap_or(0);
                ok(
                    "tls handshake (rustls)",
                    format!(
                        "{:?} (alpn {negotiated}, peer cert {cert_len} bytes)",
                        started.elapsed()
                    ),
                );
            }
            Ok(Err(error)) => fail("tls handshake (rustls)", error.to_string()),
            Err(_) => fail("tls handshake (rustls)", "timeout"),
        }
    }
    println!();

    // 4. HTTPS through the reqwest stack (rustls, HTTP/2 via ALPN).
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build();
    match client {
        Ok(client) => {
            let started = Instant::now();
            match client.get(HTTP_URL).send().await {
                Ok(response) => ok(
                    "https get",
                    format!(
                        "{} {:?} in {:?}",
                        response.status(),
                        response.version(),
                        started.elapsed()
                    ),
                ),
                Err(error) => fail("https get", error.to_string()),
            }
        }
        Err(error) => fail("https client", error.to_string()),
    }

    // 5. WebSocket upgrade (no token: the upstream must reject it, but only
    //    after the TLS + HTTP upgrade path proved it could speak to us).
    let ws_connector: Connector = Connector::Rustls(rustls_config());
    let mut request = match WS_URL.into_client_request() {
        Ok(request) => request,
        Err(error) => {
            fail("ws request", error.to_string());
            return;
        }
    };
    request.headers_mut().insert(
        "origin",
        "https://rollercoin.com".parse().expect("origin header"),
    );
    request
        .headers_mut()
        .insert("user-agent", USER_AGENT.parse().expect("user agent header"));

    let started = Instant::now();
    match tokio::time::timeout(
        TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(ws_connector)),
    )
    .await
    {
        Ok(Ok((_socket, response))) => ok(
            "ws handshake",
            format!("{:?} (status {})", started.elapsed(), response.status()),
        ),
        Ok(Err(error)) => {
            // A 401/403 during the upgrade still proves TLS works.
            let text = error.to_string();
            if text.contains("401") || text.contains("403") {
                ok("ws handshake", format!("rejected as expected: {text}"));
            } else {
                fail("ws handshake", text);
            }
        }
        Err(_) => fail("ws handshake", "timeout"),
    }
}
