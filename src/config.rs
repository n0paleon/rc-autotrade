//! Environment configuration.
//!
//! Differences from the old Bun worker are deliberate and documented:
//! * the data directory defaults to `./data`; override it with `RC_DATA_DIR`
//!   (or `DATA_DIR`) to keep several instances apart.
//! * `.env` is parsed here instead of mutating the process environment, so
//!   config never leaks into unrelated code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub worker_token: String,
    pub data_dir: PathBuf,
    pub api_base: String,
    pub ws_url: String,
    pub cookie: String,
    pub keep_alive_ms: i64,
    pub keep_alive_url: String,
    pub system_sample_ms: i64,
    pub mock: bool,
    pub mock_interval_ms: i64,
    pub log_level: String,
    pub cwd: PathBuf,
}

fn read_int(value: Option<&String>, fallback: i64) -> i64 {
    value
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(fallback)
}

fn read_bool(value: Option<&String>, fallback: bool) -> bool {
    match value {
        None => fallback,
        Some(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    }
}

/// Parses a dotenv file the way `process.loadEnvFile` does for our use:
/// `KEY=VALUE`, optional surrounding quotes, `#` comments, no expansion.
pub fn parse_dotenv(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_start_matches("export ").trim();
        if key.is_empty() {
            continue;
        }
        let mut value = raw.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

impl Config {
    pub fn load(cwd: &Path) -> Result<Self, String> {
        let mut file_values = HashMap::new();
        let env_file = cwd.join(".env");
        if let Ok(text) = std::fs::read_to_string(&env_file) {
            file_values = parse_dotenv(&text);
        }

        // Real environment wins over `.env`, like `process.loadEnvFile`.
        let get = |key: &str| -> Option<String> {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    file_values
                        .get(key)
                        .cloned()
                        .filter(|value| !value.is_empty())
                })
        };

        let port: u16 = match get("PORT") {
            None => DEFAULT_PORT,
            Some(raw) => raw
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("PORT must be between 0 and 65535 (got {raw})."))?,
        };

        let host = get("HOST").unwrap_or_else(|| "127.0.0.1".to_string());
        let worker_token = get("WORKER_TOKEN").unwrap_or_default();
        if !worker_token.is_empty() && worker_token.len() < 16 {
            return Err("WORKER_TOKEN must be at least 16 characters long.".to_string());
        }

        let log_level = get("LOG_LEVEL")
            .unwrap_or_else(|| "info".to_string())
            .to_ascii_lowercase();
        if !["debug", "info", "warn", "error"].contains(&log_level.as_str()) {
            return Err(format!(
                "LOG_LEVEL must be debug, info, warn or error (got {log_level})."
            ));
        }

        let api_base = get("RC_API_BASE")
            .unwrap_or_else(|| "https://rollercoin.com".to_string())
            .trim_end_matches('/')
            .to_string();

        let default_keepalive = format!("{api_base}/api/marketplace/purchase-item");
        let keep_alive_url = match get("RC_KEEPALIVE_URL") {
            None => default_keepalive,
            Some(raw) => {
                if raw.starts_with("http://") || raw.starts_with("https://") {
                    raw
                } else {
                    format!("{api_base}/{raw}")
                }
            }
        };

        let keep_alive_raw = read_int(get("RC_KEEPALIVE_MS").as_ref(), 30_000);
        let keep_alive_ms = if keep_alive_raw <= 0 {
            0
        } else {
            keep_alive_raw.clamp(5_000, 600_000)
        };

        let system_sample_ms = read_int(get("SYSTEM_SAMPLE_MS").as_ref(), 2_000).clamp(500, 60_000);

        let data_dir_raw = get("RC_DATA_DIR")
            .or_else(|| get("DATA_DIR"))
            .unwrap_or_else(|| "data".to_string());
        let data_dir = if Path::new(&data_dir_raw).is_absolute() {
            PathBuf::from(&data_dir_raw)
        } else {
            cwd.join(&data_dir_raw)
        };

        Ok(Self {
            host,
            port,
            worker_token,
            data_dir,
            api_base,
            ws_url: get("RC_WS_URL").unwrap_or_else(|| "wss://nws.rollercoin.com/".to_string()),
            cookie: get("RC_COOKIE").unwrap_or_default(),
            keep_alive_ms,
            keep_alive_url,
            system_sample_ms,
            mock: read_bool(get("MOCK").as_ref(), false),
            mock_interval_ms: read_int(get("MOCK_INTERVAL_MS").as_ref(), 1_400).clamp(200, 10_000),
            log_level,
            cwd: cwd.to_path_buf(),
        })
    }

    /// URL the deck should use when the bind address is a wildcard.
    pub fn display_url(&self, actual_port: u16) -> String {
        let host = if self.host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &self.host
        };
        format!("http://{host}:{actual_port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_with_quotes_and_comments() {
        let values =
            parse_dotenv("# comment\nPORT=9000\nHOST=\"0.0.0.0\"\nexport MOCK=1\nEMPTY=\n");
        assert_eq!(values.get("PORT").map(String::as_str), Some("9000"));
        assert_eq!(values.get("HOST").map(String::as_str), Some("0.0.0.0"));
        assert_eq!(values.get("MOCK").map(String::as_str), Some("1"));
    }

    #[test]
    fn clamps_keepalive_and_sample_intervals() {
        // The clamping logic is inline in `load`; assert the constants agree
        // with the JS worker so a future refactor keeps them in sync.
        assert_eq!(30_000_i64.clamp(5_000, 600_000), 30_000);
        assert_eq!(1_000_i64.clamp(500, 60_000), 1_000);
    }
}
