//! One line, one compact JSON-RPC message. The writing half is shared by every thread; requests we send wait for their
//! answers by id. Gathered from the `wire.rs` that miyu-memes and miyu-render each carried.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

/// An answer: `result`, or the JSON-RPC `error` as it came.
pub(crate) type Answer = Result<Value, Value>;

/// The connection to the core.
pub(crate) struct Wire {
    out: Mutex<Box<dyn Write + Send>>,
    waiting: Mutex<HashMap<String, Sender<Answer>>>,
    next: AtomicU64,
    /// Starts every request id: this process and when it started. The core answers a request whose id it saw before in the
    /// same session (the last 1024) with the earlier answer and does nothing, across connections; plain `x1, x2…` would make
    /// a restarted extension's first `session.send` calls silently replay old ones.
    prefix: String,
}

impl Wire {
    pub(crate) fn new(out: Box<dyn Write + Send>) -> Wire {
        Wire {
            out: Mutex::new(out),
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            prefix: id_prefix(),
        }
    }

    /// Write one line. If the core closed its end there is nothing to do: the reading half reaches the end and `run`
    /// returns.
    pub(crate) fn send(&self, line: &Value) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            // The core is gone.
        }
    }

    /// Send a request and wait up to `wait` for its answer; no answer counts as an error.
    pub(crate) fn call(&self, method: &str, params: &Value, wait: Duration) -> Answer {
        let id = format!(
            "{}{}",
            self.prefix,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let (tell, answer) = mpsc::channel();
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), tell);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let got = answer
            .recv_timeout(wait)
            .unwrap_or_else(|_| Err(json!({"message": format!("{method} had no answer in time")})));
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        got
    }

    /// An answer arrived: hand it to whoever waits for `id`; unknown ids are ignored.
    pub(crate) fn deliver(&self, id: &str, answer: Answer) {
        let tell = self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        if let Some(tell) = tell {
            drop(tell.send(answer));
        }
    }

    /// Answer the core's request `id`.
    pub(crate) fn answer(&self, id: &Value, answer: Answer) {
        let line = match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        self.send(&line);
    }
}

/// JSON-RPC's "method not found" (`-32601`).
pub(crate) fn not_found(method: &str) -> Value {
    json!({"code": -32601, "message": format!("no handler for {method}")})
}

/// `x<process id><nanoseconds since 1970>-` in hex, and a count of wires made in this process: unique across restarts.
fn id_prefix() -> String {
    static MADE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!(
        "x{:x}{nanos:x}{:x}-",
        std::process::id(),
        MADE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::Wire;

    #[test]
    fn request_ids_differ_between_starts() {
        let first = Wire::new(Box::new(std::io::sink()));
        let second = Wire::new(Box::new(std::io::sink()));
        assert!(first.prefix.starts_with('x') && first.prefix.ends_with('-'));
        assert_ne!(
            first.prefix, second.prefix,
            "a restarted extension must not reuse ids"
        );
    }
}
