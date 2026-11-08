//! DNS telemetry for the loot latency debugger.
//!
//! [`RecordingResolver`] is installed on the shared `reqwest` client, so it
//! sees the resolver work the order actually pays for:
//!
//! * hyper only asks for DNS when it opens a connection, so a purchase that
//!   never touched the resolver reused a pooled keep-alive socket
//!   ([`SocketMode::Reused`]);
//! * when a connection is opened, the resolver reports the real lookup
//!   latency, address and address family ([`DnsLookup`]), not a parallel
//!   guess made by a separate probe task.
//!
//! Attribution is per request, not global: [`DnsRecorder::begin`] opens a
//! window, and while the HTTP call is polled the task-local [`REQUEST_ID`]
//! tells the resolver which window a lookup belongs to. Concurrent purchases,
//! the warm probe and label enrichment therefore cannot leak samples into each
//! other. Lookups outside any window are ignored.
//!
//! IP-literal upstreams are connected to directly by hyper, so the resolver is
//! never called and the mode stays [`SocketMode::Unknown`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Whether the purchase had to open a connection or could reuse a pooled one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SocketMode {
    /// Not observable (IP literal upstream, mock run, no HTTP call, a proxied
    /// connection, or an exchange that never produced a response).
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

/// Everything one request's resolver window collected.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DnsSnapshot {
    /// Lookups started in this window, including ones that never completed.
    pub attempted: u32,
    /// Completed lookups, oldest first.
    pub samples: Vec<DnsLookup>,
}

impl DnsSnapshot {
    /// A single completed lookup. More than one attempt is ambiguous, so the
    /// caller gets no sample rather than a guess.
    pub fn single_sample(&self) -> Option<DnsLookup> {
        if self.attempted == 1 && self.samples.len() == 1 {
            self.samples.first().cloned()
        } else {
            None
        }
    }

    /// The newest completed sample, used to keep a resolver error visible even
    /// when the exchange produced no response.
    pub fn last_completed(&self) -> Option<DnsLookup> {
        self.samples.last().cloned()
    }
}

tokio::task_local! {
    /// The request window the current task's HTTP call belongs to.
    static REQUEST_ID: u64;
}

/// Records the resolver work of the request whose task-local window is active.
#[derive(Debug, Default)]
pub struct DnsRecorder {
    next_id: AtomicU64,
    requests: Mutex<HashMap<u64, DnsSnapshot>>,
}

impl DnsRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a window for one request. The returned guard removes the window
    /// when dropped, so cancelled or timed-out calls do not accumulate state.
    pub fn begin(self: &Arc<Self>) -> DnsRequestGuard {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, DnsSnapshot::default());
        DnsRequestGuard {
            recorder: Arc::clone(self),
            id,
        }
    }

    fn snapshot(&self, id: u64) -> DnsSnapshot {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    fn with_current<F: FnOnce(&mut DnsSnapshot)>(&self, update: F) {
        let Ok(id) = REQUEST_ID.try_with(|id| *id) else {
            // The warm probe, label enrichment and any other traffic without a
            // window must not pollute a real request's sample.
            return;
        };
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(snapshot) = requests.get_mut(&id) {
            update(snapshot);
        }
    }

    /// Called by the resolver the moment it starts, before the blocking lookup.
    fn start(&self) {
        self.with_current(|snapshot| {
            snapshot.attempted = snapshot.attempted.saturating_add(1);
        });
    }

    /// Stores a completed lookup. Only the newest sequence wins is gone: each
    /// request owns its own list, so ordering between requests no longer
    /// matters.
    fn finish(&self, lookup: DnsLookup) {
        self.with_current(|snapshot| snapshot.samples.push(lookup));
    }
}

/// The window one HTTP call owns. Drop it once the call is done.
#[derive(Debug)]
pub struct DnsRequestGuard {
    recorder: Arc<DnsRecorder>,
    id: u64,
}

impl DnsRequestGuard {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn snapshot(&self) -> DnsSnapshot {
        self.recorder.snapshot(self.id)
    }

    /// Polls `future` with this request's window installed, so any resolver
    /// call it makes is attributed here. hyper-util polls the connector inside
    /// the request task, which is what makes this precise.
    pub async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        REQUEST_ID.scope(self.id, future).await
    }
}

impl Drop for DnsRequestGuard {
    fn drop(&mut self) {
        self.recorder
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
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
            recorder.start();
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
                    recorder.finish(lookup);
                    Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
                }
                Err(error) => {
                    recorder.finish(DnsLookup {
                        ms: elapsed,
                        address: None,
                        family: None,
                        error: Some(short_error(&error.to_string())),
                    });
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

    fn lookup(ms: f64, address: &str) -> DnsLookup {
        DnsLookup {
            ms,
            address: Some(address.to_string()),
            family: Some(4),
            error: None,
        }
    }

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

    #[tokio::test]
    async fn attributes_a_lookup_to_the_request_window() {
        let recorder = Arc::new(DnsRecorder::new());
        let guard = recorder.begin();
        assert_eq!(guard.snapshot(), DnsSnapshot::default());

        guard
            .scope(async {
                recorder.start();
                recorder.finish(lookup(4.5, "104.18.0.1"));
            })
            .await;

        let snapshot = guard.snapshot();
        assert_eq!(snapshot.attempted, 1);
        assert_eq!(snapshot.single_sample().map(|sample| sample.ms), Some(4.5));
    }

    #[tokio::test]
    async fn concurrent_windows_do_not_share_samples() {
        let recorder = Arc::new(DnsRecorder::new());
        let first = recorder.begin();
        let second = recorder.begin();

        first
            .scope(async {
                recorder.start();
                recorder.finish(lookup(2.0, "1.1.1.1"));
            })
            .await;
        second
            .scope(async {
                recorder.start();
                recorder.finish(lookup(9.0, "9.9.9.9"));
            })
            .await;

        assert_eq!(
            first
                .snapshot()
                .single_sample()
                .and_then(|sample| sample.address),
            Some("1.1.1.1".to_string())
        );
        assert_eq!(
            second
                .snapshot()
                .single_sample()
                .and_then(|sample| sample.address),
            Some("9.9.9.9".to_string())
        );
    }

    #[tokio::test]
    async fn traffic_outside_a_window_is_ignored() {
        let recorder = Arc::new(DnsRecorder::new());
        let guard = recorder.begin();

        // The warm probe and label enrichment run without a window.
        recorder.start();
        recorder.finish(lookup(50.0, "8.8.8.8"));

        assert_eq!(guard.snapshot(), DnsSnapshot::default());
    }

    #[tokio::test]
    async fn a_started_but_unfinished_lookup_is_visible() {
        let recorder = Arc::new(DnsRecorder::new());
        let guard = recorder.begin();
        guard.scope(async { recorder.start() }).await;

        let snapshot = guard.snapshot();
        assert_eq!(snapshot.attempted, 1);
        assert!(snapshot.samples.is_empty());
        assert!(snapshot.single_sample().is_none());
    }

    #[tokio::test]
    async fn two_attempts_drop_the_ambiguous_sample() {
        let recorder = Arc::new(DnsRecorder::new());
        let guard = recorder.begin();
        guard
            .scope(async {
                recorder.start();
                recorder.finish(lookup(3.0, "1.1.1.1"));
                recorder.start();
                recorder.finish(lookup(4.0, "1.0.0.1"));
            })
            .await;

        let snapshot = guard.snapshot();
        assert_eq!(snapshot.attempted, 2);
        assert!(snapshot.single_sample().is_none());
        assert_eq!(snapshot.last_completed().map(|sample| sample.ms), Some(4.0));
    }

    #[tokio::test]
    async fn dropping_a_window_removes_it() {
        let recorder = Arc::new(DnsRecorder::new());
        let guard = recorder.begin();
        let id = guard.id();
        assert!(recorder.requests.lock().unwrap().contains_key(&id));
        drop(guard);
        assert!(!recorder.requests.lock().unwrap().contains_key(&id));
    }

    #[tokio::test]
    async fn resolver_records_the_real_lookup_and_returns_every_address() {
        let recorder = Arc::new(DnsRecorder::new());
        let resolver = RecordingResolver::new(recorder.clone());
        let guard = recorder.begin();
        let name: reqwest::dns::Name = "localhost".parse().expect("name");
        let addresses: Vec<std::net::SocketAddr> = guard
            .scope(resolver.resolve(name))
            .await
            .expect("resolve")
            .collect();
        assert!(!addresses.is_empty(), "localhost must resolve");

        let snapshot = guard.snapshot();
        assert_eq!(
            snapshot.attempted, 1,
            "the resolver must record the attempt"
        );
        let sample = snapshot.single_sample().expect("completed sample");
        assert!(sample.ms >= 0.0);
        assert_eq!(sample.error, None);
        let address: IpAddr = sample.address.expect("address").parse().expect("ip");
        assert!(address.is_loopback());
        assert!(matches!(sample.family, Some(4) | Some(6)));
    }
}
