use super::*;
use axum::routing::post;
use axum::{Json, Router};

async fn spawn_server(status: u16) -> String {
    let app = Router::new().route(
        "/probe",
        post(move || async move {
            if status == 429 {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "error": "slow down" })),
                )
            } else {
                (axum::http::StatusCode::OK, Json(json!({ "ok": true })))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/probe")
}

fn service(url: String, tracker: Arc<Mutex<LatencyTracker>>) -> Arc<KeepaliveService> {
    KeepaliveService::new(KeepaliveConfig {
        url,
        interval_ms: 30_000,
        timeout_ms: 2_000,
        suspend_ms: 60_000,
        jitter_ratio: 0.0,
        credentials: Arc::new(|| ("token".into(), String::new())),
        should_probe: Arc::new(|| true),
        logger: Arc::new(Logger::new("error")),
        latency: tracker,
        last_request_at: Arc::new(AtomicI64::new(0)),
        http: reqwest::Client::new(),
    })
}

fn tracker() -> Arc<Mutex<LatencyTracker>> {
    Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))))
}

#[tokio::test]
async fn a_200_probe_records_a_latency_sample() {
    let tracker = tracker();
    let service = service(spawn_server(200).await, tracker.clone());
    let result = service.probe_once().await;
    assert!(result.ok);
    assert_eq!(result.status, 200);
    assert!(result.rtt_ms.is_some());

    let snapshot = tracker.lock().unwrap().snapshot();
    assert_eq!(snapshot["channels"]["keepalive"]["count"], 1);
    assert_eq!(snapshot["channels"]["keepalive"]["lastOk"], true);

    let stats = service.stats_json();
    assert_eq!(stats["probes"], 1);
    assert_eq!(stats["lastStatus"], 200);
    assert_eq!(stats["failures"], 0);
}

#[tokio::test]
async fn a_429_suspends_further_probes() {
    let service = service(spawn_server(429).await, tracker());
    let result = service.probe_once().await;
    assert!(result.ok, "any HTTP status counts as a reached upstream");
    assert_eq!(result.status, 429);
    assert!(result.suspended_until.is_some());
    let stats = service.stats_json();
    assert!(stats["suspendedUntil"].as_i64().is_some());

    // The next tick must wait for the suspension window, not probe again.
    let delay = service.tick().await;
    assert!(delay > 30_000, "expected suspension delay, got {delay}");
}

#[tokio::test]
async fn transport_failure_backs_off() {
    let service = service("http://127.0.0.1:1/probe".into(), tracker());
    let result = service.probe_once().await;
    assert!(!result.ok);
    assert!(result.error.is_some());
    let stats = service.stats_json();
    assert_eq!(stats["failures"], 1);
    assert_eq!(stats["consecutiveFailures"], 1);

    let delay = service.tick().await;
    assert!(delay >= 60_000, "backoff should double, got {delay}");
}

#[tokio::test]
async fn skips_when_real_traffic_just_happened() {
    let last = Arc::new(AtomicI64::new(now_ms()));
    let mut config_service = KeepaliveService::new(KeepaliveConfig {
        url: "http://127.0.0.1:1/probe".into(),
        interval_ms: 30_000,
        timeout_ms: 1_000,
        suspend_ms: 60_000,
        jitter_ratio: 0.0,
        credentials: Arc::new(|| (String::new(), String::new())),
        should_probe: Arc::new(|| true),
        logger: Arc::new(Logger::new("error")),
        latency: tracker(),
        last_request_at: last.clone(),
        http: reqwest::Client::new(),
    });
    let delay = config_service.tick().await;
    assert!(delay > 0);
    let stats = config_service.stats_json();
    assert!(stats["lastSkipAt"].as_i64().is_some());
    assert_eq!(stats["probes"], 0);
    let _ = &mut config_service;
}

#[tokio::test]
async fn disabled_service_reports_skipped() {
    let service = KeepaliveService::new(KeepaliveConfig {
        url: String::new(),
        interval_ms: 0,
        timeout_ms: 1_000,
        suspend_ms: 60_000,
        jitter_ratio: 0.0,
        credentials: Arc::new(|| (String::new(), String::new())),
        should_probe: Arc::new(|| true),
        logger: Arc::new(Logger::new("error")),
        latency: tracker(),
        last_request_at: Arc::new(AtomicI64::new(0)),
        http: reqwest::Client::new(),
    });
    assert!(!service.enabled());
    let result = service.probe_once().await;
    assert!(result.skipped);
    assert!(service.start().is_none());
}
