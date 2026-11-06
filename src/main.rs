//! Rust worker binary (the backend).
//!
//! Run from the repository root: `bun run start` (release), or directly
//! `cargo run --release` with optional `PORT` / `RC_DATA_DIR` overrides.
//! The `dist/` deck and `.env` are resolved relative to the working directory.
//!
//! All logic lives in the library crate (`src/lib.rs`) so tests and examples
//! exercise exactly what ships.

use std::path::PathBuf;
use std::sync::Arc;

use rollercoin_worker::api::ApiClient;
use rollercoin_worker::config::Config;
use rollercoin_worker::constants::{APP_NAME, APP_VERSION};
use rollercoin_worker::engine::Engine;
use rollercoin_worker::latency::LatencyTracker;
use rollercoin_worker::logger::Logger;
use rollercoin_worker::runtime::Worker;
use rollercoin_worker::server::AppState;
use rollercoin_worker::store::Store;
use rollercoin_worker::system::SystemMonitor;

const WORKER_HEADER: &str = r#"
  ______               ____  ____
 /_  __/_  __ ______  / __ \/ ____|
  / / / / / / ___/ _ \/ /_/ / /
 / / / /_/ / /  /  __/ _, _/ /___
/_/  \__,_/_/   \___/_/ |_|\____/   rust worker
"#;

fn print_banner(config: &Config, store: &Store, generated_token: Option<&str>) {
    println!("{WORKER_HEADER}");
    println!("  {APP_NAME} {APP_VERSION} (Rust)");
    println!("  data      : {}", store.data_dir.display());
    println!("  upstream  : {}", config.api_base);
    println!(
        "  access key: {}",
        store
            .state
            .worker_token_preview
            .clone()
            .unwrap_or_else(|| "not set yet".to_string())
    );
    if let Some(token) = generated_token {
        println!();
        println!("  ┌──────────────────────────────────────────────────────────────┐");
        println!("  │ A new worker access key was generated. Copy it now:          │");
        println!("  └──────────────────────────────────────────────────────────────┘");
        println!("  {token}");
        println!("  (stored as a hash; it cannot be shown again)");
    }
    println!();
}

async fn run() -> Result<(), String> {
    let cwd: PathBuf = std::env::current_dir()
        .map_err(|error| format!("could not resolve the working directory: {error}"))?;
    let config = Config::load(&cwd)?;

    let logger = Arc::new(Logger::new(&config.log_level));
    logger.info("worker", format!("{APP_NAME} {APP_VERSION} (Rust) booting"));

    let mut store = Store::open(config.data_dir.clone(), logger.clone())?;
    let generated = store.ensure_worker_token(&config.worker_token)?;

    let mut engine = Engine::new(rollercoin_worker::engine::wall_clock(), logger.clone());
    let restored = engine.hydrate_market(&store);
    if restored == 0 {
        logger.debug("engine", "no cached market snapshots to restore");
    }

    if config.mock {
        rollercoin_worker::runtime::seed_mock_state(&mut store, &logger)?;
    }

    print_banner(&config, &store, generated.as_deref());

    let latency = Arc::new(std::sync::Mutex::new(LatencyTracker::new(
        120,
        Arc::new(rollercoin_worker::store::now_ms),
    )));
    let api =
        Arc::new(ApiClient::with_default_timeout(&config.api_base).with_latency(latency.clone()));

    let system = Arc::new(std::sync::Mutex::new(SystemMonitor::new(
        config.system_sample_ms.max(500) as u64,
        store.db_metrics(),
    )));

    let item_info = Arc::new(rollercoin_worker::item_info::ItemInfoService::new(
        api.clone(),
        logger.clone(),
    ));

    let state = Arc::new(AppState::new(
        config.clone(),
        store,
        engine,
        api,
        item_info,
        latency,
        system,
        logger.clone(),
    ));
    let app = rollercoin_worker::server::router(state.clone(), cwd.join("dist"));

    // Release pending purchases whose confirmation never arrived (120 s).
    let prune_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            ticker.tick().await;
            prune_state
                .engine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .prune_pending();
        }
    });

    let address = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|error| format!("could not bind {address}: {error}"))?;
    let actual_port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(config.port);
    state.set_port(actual_port);

    logger.info(
        "worker",
        format!("control deck on {}", config.display_url(actual_port)),
    );
    if config.mock {
        logger.warn(
            "worker",
            "MOCK=1 — the simulator is active; no real orders will be sent",
        );
    }

    let worker = Worker::start(state.clone());
    logger.info("worker", "market feed + engine loop started");

    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(logger.clone()))
        .await
        .map_err(|error| format!("server error: {error}"))?;

    worker.stop().await;

    {
        let mut store = shutdown_state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut engine = shutdown_state
                .engine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            engine.flush_market_cache(&store);
        }
        store.close();
    }
    logger.info("worker", "bye");
    Ok(())
}

/// Resolves on Ctrl+C (`SIGINT`) or `SIGTERM` (what systemd and containers send),
/// so an orchestrator stop still flushes the market cache and closes SQLite.
async fn shutdown_signal(logger: Arc<Logger>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(error) => {
                logger.warn(
                    "worker",
                    format!("could not install SIGTERM handler ({error}); SIGINT only"),
                );
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    logger.info("worker", "shutdown signal received");
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}
