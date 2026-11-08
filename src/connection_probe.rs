//! Direct connection telemetry for the loot latency debugger.
//!
//! The DNS recorder answers a related but weaker question: "did this request
//! ask the resolver?" That inference breaks for IP-literal upstreams and for
//! proxy scenarios. This module observes the HTTP client's connector instead:
//! a `tower` layer sees every attempt to establish a new TCP connection, before
//! hyper's pool can serve a request from an existing one.
//!
//! It is still a recorder, not a per-request tag: hyper decides pool reuse
//! before the connector is called, so an increase in the counters during a
//! request window is the signal. That is strictly closer to the transport than
//! the resolver heuristic and also works when no DNS lookup is involved.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tower::{Layer, Service};

/// Counts connection attempts and successful connection establishments.
#[derive(Debug, Default)]
pub struct ConnectionRecorder {
    attempted: AtomicU64,
    opened: AtomicU64,
}

impl ConnectionRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot to compare against after one request.
    pub fn mark(&self) -> ConnectionMark {
        ConnectionMark {
            attempted: self.attempted.load(Ordering::SeqCst),
            opened: self.opened.load(Ordering::SeqCst),
        }
    }

    /// Whether this window saw a completed connection or only an attempt.
    pub fn since(&self, mark: ConnectionMark) -> ConnectionDelta {
        ConnectionDelta {
            opened: self.opened.load(Ordering::SeqCst) > mark.opened,
            attempted: self.attempted.load(Ordering::SeqCst) > mark.attempted,
        }
    }

    fn attempt(&self) {
        self.attempted.fetch_add(1, Ordering::SeqCst);
    }

    fn opened(&self) {
        self.opened.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counters captured before a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionMark {
    attempted: u64,
    opened: u64,
}

/// What happened between a mark and the end of a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionDelta {
    pub opened: bool,
    pub attempted: bool,
}

/// Wraps reqwest's connector. Each `call` is a new connection attempt; hyper
/// never calls it when it can serve the request from its pool.
#[derive(Clone)]
pub struct ConnectionCounterLayer {
    recorder: Arc<ConnectionRecorder>,
}

impl ConnectionCounterLayer {
    pub fn new(recorder: Arc<ConnectionRecorder>) -> Self {
        Self { recorder }
    }
}

impl<S> Layer<S> for ConnectionCounterLayer {
    type Service = ConnectionCounterService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ConnectionCounterService {
            inner,
            recorder: self.recorder.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ConnectionCounterService<S> {
    inner: S,
    recorder: Arc<ConnectionRecorder>,
}

impl<S, R> Service<R> for ConnectionCounterService<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        let future = self.inner.call(request);
        let recorder = self.recorder.clone();
        recorder.attempt();
        Box::pin(async move {
            let result = future.await;
            if result.is_ok() {
                recorder.opened();
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Ready, ready};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    struct FakeConnector {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl Service<&'static str> for FakeConnector {
        type Response = &'static str;
        type Error = &'static str;
        type Future = Ready<Result<&'static str, &'static str>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: &'static str) -> Self::Future {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ready(if self.fail {
                Err("connect failed")
            } else {
                Ok("connected")
            })
        }
    }

    #[tokio::test]
    async fn records_a_successful_connection() {
        let recorder = Arc::new(ConnectionRecorder::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = ConnectionCounterLayer::new(recorder.clone())
            .layer(FakeConnector { calls, fail: false });

        let mark = recorder.mark();
        assert_eq!(service.call("host").await, Ok("connected"));
        let delta = recorder.since(mark);
        assert!(delta.opened);
        assert!(delta.attempted);
    }

    #[tokio::test]
    async fn records_an_attempt_that_failed() {
        let recorder = Arc::new(ConnectionRecorder::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut service = ConnectionCounterLayer::new(recorder.clone())
            .layer(FakeConnector { calls, fail: true });

        let mark = recorder.mark();
        assert_eq!(service.call("host").await, Err("connect failed"));
        let delta = recorder.since(mark);
        assert!(!delta.opened);
        assert!(delta.attempted);
    }
}
