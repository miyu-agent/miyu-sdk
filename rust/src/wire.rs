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
}

impl Wire {
    pub(crate) fn new(out: Box<dyn Write + Send>) -> Wire {
        Wire {
            out: Mutex::new(out),
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
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
        let id = format!("x{}", self.next.fetch_add(1, Ordering::Relaxed));
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
