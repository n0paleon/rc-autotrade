use super::purchase::apply_dns_telemetry;
use crate::api::PurchaseResult;
use crate::dns_probe::{DnsLookup, SocketMode};
use crate::timings::Timings;

fn purchase(socket: SocketMode, dns: Option<DnsLookup>) -> PurchaseResult {
    PurchaseResult {
        socket,
        dns,
        ..Default::default()
    }
}

#[test]
fn a_new_connection_keeps_the_real_lookup() {
    let mut timings = Timings::default();
    let result = purchase(
        SocketMode::New,
        Some(DnsLookup {
            ms: 18.25,
            address: Some("104.18.0.1".into()),
            family: Some(4),
            error: None,
        }),
    );
    apply_dns_telemetry(&mut timings, Some("rollercoin.com"), &result);
    assert_eq!(timings.socket_mode.as_deref(), Some("new"));
    assert_eq!(timings.dns_host.as_deref(), Some("rollercoin.com"));
    assert_eq!(timings.dns_mode.as_deref(), Some("lookup"));
    assert_eq!(timings.dns_ms, Some(18.25));
    assert_eq!(timings.dns_address.as_deref(), Some("104.18.0.1"));
    assert_eq!(timings.dns_family, Some(4));
    assert_eq!(timings.dns_error, None);
}

#[test]
fn a_reused_socket_reports_no_dns_cost() {
    let mut timings = Timings::default();
    apply_dns_telemetry(
        &mut timings,
        Some("rollercoin.com"),
        &purchase(SocketMode::Reused, None),
    );
    assert_eq!(timings.socket_mode.as_deref(), Some("reused"));
    assert_eq!(timings.dns_mode.as_deref(), Some("reused"));
    assert_eq!(timings.dns_ms, None);
    assert_eq!(timings.dns_address, None);
}

#[test]
fn local_and_ip_upstreams_are_skipped() {
    let mut timings = Timings::default();
    apply_dns_telemetry(
        &mut timings,
        Some("127.0.0.1"),
        &purchase(SocketMode::Unknown, None),
    );
    assert_eq!(timings.socket_mode.as_deref(), Some("unknown"));
    assert_eq!(timings.dns_mode.as_deref(), Some("skipped"));
    assert_eq!(timings.dns_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(timings.dns_ms, None);
}

#[test]
fn a_failed_lookup_keeps_its_duration_and_error() {
    let mut timings = Timings::default();
    let result = purchase(
        SocketMode::New,
        Some(DnsLookup {
            ms: 3.5,
            address: None,
            family: None,
            error: Some("lookup timed out".into()),
        }),
    );
    apply_dns_telemetry(&mut timings, Some("rollercoin.com"), &result);
    assert_eq!(timings.dns_mode.as_deref(), Some("lookup"));
    assert_eq!(timings.dns_ms, Some(3.5));
    assert_eq!(timings.dns_error.as_deref(), Some("lookup timed out"));
}
