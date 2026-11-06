//! Rotates the worker access key and prints the new plaintext exactly once.
//!
//! ```text
//! bun run key:reset     # cargo run --release --bin reset_token
//! ```
//!
//! The key is stored as a hash in SQLite (`meta.worker_token_hash`); settings,
//! targets and history are untouched. Refuses to run when `WORKER_TOKEN` is set,
//! because the next boot would overwrite the rotated hash with the env value.

use std::sync::Arc;

use rollercoin_worker::config::Config;
use rollercoin_worker::logger::Logger;
use rollercoin_worker::store::Store;

fn main() {
    if let Err(error) = run() {
        eprintln!("fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("could not resolve the working directory: {error}"))?;
    let config = Config::load(&cwd)?;

    if !config.worker_token.is_empty() {
        return Err(
            "WORKER_TOKEN is set in the environment or .env; remove it before rotating the key"
                .to_string(),
        );
    }

    let logger = Arc::new(Logger::new(&config.log_level));
    let mut store = Store::open(config.data_dir.clone(), logger)?;
    let token = store.rotate_worker_token()?;
    store.close();

    println!("new worker access key (stored as a hash, it cannot be shown again):");
    println!("{token}");
    println!();
    println!("Restart the worker, then unlock the deck with this key.");
    Ok(())
}
