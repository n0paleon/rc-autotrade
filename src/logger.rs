//! Console + in-memory + socket logger.
//!
//! The control deck receives every line through a sink installed by the HTTP
//! server, and clients that connect late still get the tail of the ring buffer.

use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::clock::now_ms;
use crate::constants::LOG_BUFFER_SIZE;
use crate::lock::lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Warn = 30,
    Error = 40,
}

impl Level {
    pub fn parse(value: &str) -> Level {
        match value.to_ascii_lowercase().as_str() {
            "debug" => Level::Debug,
            "warn" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Level::Debug => "\x1b[38;5;245m",
            Level::Info => "\x1b[38;5;81m",
            Level::Warn => "\x1b[38;5;220m",
            Level::Error => "\x1b[38;5;203m",
        }
    }
}

pub type Sink = Arc<dyn Fn(Value) + Send + Sync>;

pub struct Logger {
    threshold: Mutex<Level>,
    buffer: Mutex<VecDeque<Value>>,
    sink: Mutex<Option<Sink>>,
    color: bool,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Logger")
            .field("threshold", &*lock(&self.threshold))
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

fn clock_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

impl Logger {
    pub fn new(level: &str) -> Self {
        Self {
            threshold: Mutex::new(Level::parse(level)),
            buffer: Mutex::new(VecDeque::with_capacity(LOG_BUFFER_SIZE)),
            sink: Mutex::new(None),
            color: std::io::stdout().is_terminal(),
            next_id: Mutex::new(1),
        }
    }

    pub fn set_sink(&self, sink: Sink) {
        if let Ok(mut guard) = self.sink.lock() {
            *guard = Some(sink);
        }
    }

    /// Most recent `limit` entries, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<Value> {
        let guard = lock(&self.buffer);
        let skip = guard.len().saturating_sub(limit);
        guard.iter().skip(skip).cloned().collect()
    }

    /// Whether a record at `level` would currently be kept.
    ///
    /// The frame path uses this to skip building a debug line at all when
    /// `LOG_LEVEL=info`, so extra verbosity costs nothing unless it is asked for.
    pub fn enabled(&self, level: Level) -> bool {
        level >= *lock(&self.threshold)
    }

    pub fn debug_enabled(&self) -> bool {
        self.enabled(Level::Debug)
    }

    pub fn debug(&self, scope: &str, message: impl AsRef<str>) {
        self.emit(Level::Debug, scope, message.as_ref());
    }

    /// [`Self::debug`] with a lazily built message: the closure only runs when
    /// debug is enabled. Use this on per-frame/per-message paths where
    /// `format!` would otherwise be paid at every level.
    pub fn debug_lazy(&self, scope: &str, message: impl FnOnce() -> String) {
        if self.enabled(Level::Debug) {
            self.emit(Level::Debug, scope, &message());
        }
    }

    pub fn info(&self, scope: &str, message: impl AsRef<str>) {
        self.emit(Level::Info, scope, message.as_ref());
    }

    pub fn warn(&self, scope: &str, message: impl AsRef<str>) {
        self.emit(Level::Warn, scope, message.as_ref());
    }

    pub fn error(&self, scope: &str, message: impl AsRef<str>) {
        self.emit(Level::Error, scope, message.as_ref());
    }

    fn emit(&self, level: Level, scope: &str, message: &str) {
        let threshold = *lock(&self.threshold);
        if level < threshold {
            return;
        }

        let id = {
            let mut guard = lock(&self.next_id);
            let id = *guard;
            *guard += 1;
            id
        };
        let entry = json!({
            "id": id,
            "at": now_ms(),
            "level": level.label().to_ascii_lowercase(),
            "scope": scope,
            "message": message,
        });

        {
            let mut buffer = lock(&self.buffer);
            buffer.push_back(entry.clone());
            while buffer.len() > LOG_BUFFER_SIZE {
                buffer.pop_front();
            }
        }

        let line = if self.color {
            format!(
                "\x1b[2m{}\x1b[0m {}{}\x1b[0m \x1b[38;5;177m{:<8}\x1b[0m {}",
                clock_hms(),
                level.color(),
                level.label(),
                scope,
                message
            )
        } else {
            format!(
                "{} {:<5} {:<8} {}",
                clock_hms(),
                level.label(),
                scope,
                message
            )
        };

        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();

        // Clone the sink and release the lock before calling it: a slow client
        // must never deadlock the frame loop.
        let sink = lock(&self.sink).clone();
        if let Some(sink) = sink {
            sink(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn filters_below_the_threshold() {
        let logger = Logger::new("warn");
        logger.debug("engine", "hidden");
        logger.info("engine", "hidden");
        logger.warn("engine", "shown");
        let recent = logger.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0]["level"], "warn");
    }

    #[test]
    fn forwards_entries_to_the_sink() {
        let logger = Logger::new("debug");
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        logger.set_sink(Arc::new(move |_entry| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        logger.info("store", "hello");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn keeps_the_buffer_bounded() {
        let logger = Logger::new("error");
        for i in 0..(LOG_BUFFER_SIZE + 25) {
            logger.error("test", format!("line {i}"));
        }
        assert_eq!(logger.recent(usize::MAX).len(), LOG_BUFFER_SIZE);
    }

    #[test]
    fn lazy_debug_is_only_built_when_enabled() {
        let calls = Arc::new(AtomicUsize::new(0));
        let quiet = Logger::new("info");
        let counter = calls.clone();
        quiet.debug_lazy("engine", || {
            counter.fetch_add(1, Ordering::SeqCst);
            "hidden".to_string()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(quiet.recent(10).is_empty());

        let loud = Logger::new("debug");
        let counter = calls.clone();
        loud.debug_lazy("engine", || {
            counter.fetch_add(1, Ordering::SeqCst);
            "shown".to_string()
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(loud.recent(10)[0]["message"], "shown");
    }

    #[test]
    fn reports_which_levels_are_enabled() {
        let logger = Logger::new("warn");
        assert!(!logger.debug_enabled());
        assert!(!logger.enabled(Level::Info));
        assert!(logger.enabled(Level::Warn));
        assert!(logger.enabled(Level::Error));
    }
}
