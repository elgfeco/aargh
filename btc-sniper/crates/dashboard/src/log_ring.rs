//! A thread-safe, bounded ring buffer for the dashboard log pane.
//!
//! Used as a `tracing` layer target: every log line is cloned into the ring
//! and the oldest is evicted when capacity is hit. The dashboard renders the
//! most recent N entries on every frame.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub ts: std::time::Instant,
    pub level: String,
    pub message: String,
}

#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<LogEntry>>>,
    cap: usize,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            cap,
        }
    }

    pub fn push(&self, level: impl Into<String>, message: impl Into<String>) {
        let mut g = self.inner.lock();
        if g.len() >= self.cap {
            g.pop_front();
        }
        g.push_back(LogEntry {
            ts: std::time::Instant::now(),
            level: level.into(),
            message: message.into(),
        });
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.inner.lock().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_oldest() {
        let r = LogRing::new(3);
        for i in 0..5 {
            r.push("INFO", format!("msg {i}"));
        }
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(snap[0].message.ends_with("2"));
        assert!(snap[2].message.ends_with("4"));
    }
}
