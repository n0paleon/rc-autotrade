//! DNS telemetry for the loot latency debugger.
//!
//! [`RecordingResolver`] is installed on the shared `reqwest` client, so it
//! sees the resolver work the order actually pays for:
//!
//! * hyper only asks for DNS when it opens a connection, so a purchase that
//!   never touched this resolver reused a pooled keep-alive socket
//!   ([`SocketMode::Reused`]);
//! * when a connection is opened, the resolver reports the real lookup
//!   latency, address and address family ([`DnsLookup`]), not a parallel
//!   guess made by a separate probe task.
//!
//! IP-literal upstreams are connected to directly by hyper, so the resolver is
//! never called and the mode stays [`SocketMode::Unknown`].

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Whether the purchase had to open a connection or could reuse a pooled one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SocketMode {
    /// Not observable (IP literal upstream, mock run, or no HTTP call).
    #[default]
    Unknown,
    /// The request invoked the resolver, so it opened a fresh connection.
    New,
    /// The request never touched the resolver: an idle keep-alive socket was
    /// reused from the client pool.
    Reused,
}

impl SocketMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SocketMode::Unknown => "unknown",
            SocketMode::New => "new",
            SocketMode::Reused => "reused",
        }
    }
}

/// What the resolver saw while the purchase was in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct DnsLookup {
    /// Wall time spent inside `getaddrinfo`, including a failed lookup.
    pub ms: f64,
    pub address: Option<String>,
    pub family: Option<u8>,
    pub error: Option<String>,
}

/// Records the resolver work the shared client performs so the order path can
/// tell a fresh connection from a pooled one, and can keep the real lookup.
///
/// `attempted` is bumped as soon as the resolver is invoked (before it
/// completes), which keeps connection detection correct even when a request is
/// cancelled mid-lookup. `last` stores the newest completed sample.
#[derive(Debug, Default)]
pub struct DnsRecorder {
    attempted: AtomicU64,
    last: Mutex<Option<(u64, DnsLookup)>>,
}

impl DnsRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current sequence, taken before the request is sent.
    pub fn mark(&self) -> u64 {
        self.attempted.load(Ordering::SeqCst)
    }

    /// Called by the resolver the moment it starts, before the blocking lookup.
    pub fn start(&self) -> u64 {
        self.attempted.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Stores a completed lookup. Only the newest sequence wins, so an older
    /// lookup finishing late cannot overwrite a newer one.
    pub fn finish(&self, seq: u64, lookup: DnsLookup) {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last
            .as_ref()
            .map(|(current, _)| seq >= *current)
            .unwrap_or(true)
        {
            *last = Some((seq, lookup));
        }
    }

    /// Returns whether a resolver call started, plus the newest sample taken
    /// after `mark`.
    pub fn since(&self, mark: u64) -> (bool, Option<DnsLookup>) {
        let attempted = self.attempted.load(Ordering::SeqCst);
        let last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sample = last
            .as_ref()
            .filter(|(seq, _)| *seq > mark)
            .map(|(_, lookup)| lookup.clone());
        (attempted > mark, sample)
    }
}

/// `reqwest` resolver that measures every lookup and otherwise behaves like the
/// default `getaddrinfo` resolver, including returning every address so
/// happy-eyeballs keeps working. It is only invoked for a new connection.
#[derive(Debug, Clone)]
pub struct RecordingResolver {
    recorder: Arc<DnsRecorder>,
}

impl RecordingResolver {
    pub fn new(recorder: Arc<DnsRecorder>) -> Self {
        Self { recorder }
    }
}

impl reqwest::dns::Resolve for RecordingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let recorder = self.recorder.clone();
        Box::pin(async move {
            let seq = recorder.start();
            let started = Instant::now();
            let result = tokio::net::lookup_host((host.as_str(), 0)).await;
            let elapsed = round_ms(started.elapsed());

            match result {
                Ok(addresses) => {
                    let addresses: Vec<std::net::SocketAddr> = addresses.collect();
                    let lookup = match addresses.first() {
                        Some(address) => DnsLookup {
                            ms: elapsed,
                            address: Some(address.ip().to_string()),
                            family: Some(if address.is_ipv4() { 4 } else { 6 }),
                            error: None,
                        },
                        None => DnsLookup {
                            ms: elapsed,
                            address: None,
                            family: None,
                            error: Some("lookup returned no addresses".to_string()),
                        },
                    };
                    recorder.finish(seq, lookup);
                    Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
                }
                Err(error) => {
                    recorder.finish(
                        seq,
                        DnsLookup {
                            ms: elapsed,
                            address: None,
                            family: None,
                            error: Some(short_error(&error.to_string())),
                        },
                    );
                    Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        })
    }
}

/// Hostname for telemetry, taken the way `new URL(url).hostname` does. IP
/// literals are returned without brackets so [`is_local`] can recognise them.
pub fn host_of(url: &str) -> Option<String> {
    match url::Url::parse(url).ok()?.host()? {
        url::Host::Domain(domain) => {
            let domain = domain.trim();
            (!domain.is_empty()).then(|| domain.to_string())
        }
        url::Host::Ipv4(address) => Some(address.to_string()),
        url::Host::Ipv6(address) => Some(address.to_string()),
    }
}

/// IP literals are connected to directly by hyper, so neither a resolver
/// lookup nor a reuse signal exists for them.
pub fn is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Local names and IP literals cost no resolver round-trip worth reporting.
pub fn is_local(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host == "localhost" || host.ends_with(".localhost") || is_ip(host)
}

fn round_ms(duration: Duration) -> f64 {
    let value = duration.as_secs_f64() * 1000.0;
    (value * 1_000.0).round() / 1_000.0
}

fn short_error(message: &str) -> String {
    message.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;

    #[test]
    fn extracts_the_host_the_url_way() {
        assert_eq!(
            host_of("https://rollercoin.com/api/marketplace/purchase-item"),
            Some("rollercoin.com".to_string())
        );
        assert_eq!(
            host_of("https://user:pass@example.com:8443/x"),
            Some("example.com".to_string())
        );
        assert_eq!(
            host_of("https://[2001:db8::1]/marketplace"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(
            host_of("http://127.0.0.1:8787/"),
            Some("127.0.0.1".to_string())
        );
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("file:///tmp/x"), None);
    }

    #[test]
    fn skips_local_names_and_addresses() {
        assert!(is_local("localhost"));
        assert!(is_local("deck.localhost"));
        assert!(is_local("127.0.0.1"));
        assert!(is_local("::1"));
        assert!(!is_local("rollercoin.com"));
        assert!(is_ip("127.0.0.1"));
        assert!(!is_ip("localhost"));
    }

    #[test]
    fn records_the_newest_lookup_after_a_mark() {
        let recorder = DnsRecorder::new();
        let mark = recorder.mark();
        assert_eq!(recorder.since(mark), (false, None));

        let first = recorder.start();
        recorder.finish(
            first,
            DnsLookup {
                ms: 4.5,
                address: Some("104.18.0.1".into()),
                family: Some(4),
                error: None,
            },
        );
        let (started, sample) = recorder.since(mark);
        assert!(started);
        assert_eq!(sample.as_ref().map(|lookup| lookup.ms), Some(4.5));

        // A later mark only sees work started after it.
        let later = recorder.mark();
        let (started, sample) = recorder.since(later);
        assert!(!started);
        assert!(sample.is_none());
    }

    #[test]
    fn an_older_lookup_cannot_overwrite_a_newer_one() {
        let recorder = DnsRecorder::new();
        let older = recorder.start();
        let newer = recorder.start();
        recorder.finish(
            newer,
            DnsLookup {
                ms: 2.0,
                address: Some("1.1.1.1".into()),
                family: Some(4),
                error: None,
            },
        );
        recorder.finish(
            older,
            DnsLookup {
                ms: 9.0,
                address: Some("9.9.9.9".into()),
                family: Some(4),
                error: None,
            },
        );
        let (_, sample) = recorder.since(0);
        assert_eq!(sample.map(|lookup| lookup.ms), Some(2.0));
    }

    #[tokio::test]
    async fn resolver_records_the_real_lookup_and_returns_every_address() {
        let recorder = Arc::new(DnsRecorder::new());
        let resolver = RecordingResolver::new(recorder.clone());
        let mark = recorder.mark();
        let name: reqwest::dns::Name = "localhost".parse().expect("name");
        let addresses: Vec<std::net::SocketAddr> =
            resolver.resolve(name).await.expect("resolve").collect();
        assert!(!addresses.is_empty(), "localhost must resolve");

        let (started, sample) = recorder.since(mark);
        assert!(started, "the resolver must record the attempt");
        let sample = sample.expect("completed sample");
        assert!(sample.ms >= 0.0);
        assert_eq!(sample.error, None);
        let address: IpAddr = sample.address.expect("address").parse().expect("ip");
        assert!(address.is_loopback());
        assert!(matches!(sample.family, Some(4) | Some(6)));
    }
}
