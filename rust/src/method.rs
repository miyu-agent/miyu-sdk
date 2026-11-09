//! Methods other packages, heads and pages call (`package.methods`, `method.call`): how long the core waits for each,
//! which ones are registered right away and which wait until the extension says so, and errors that carry a code.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// JSON-RPC's "server error" range: an error with a [`MethodError::code`] goes back with this number, the code in `data`.
const CODED: i64 = -32000;
/// JSON-RPC's internal error: an error without a code.
const INTERNAL: i64 = -32603;

/// A method to register: its full name (for a bus name, prefixed: `stt.transcribe`), how long the core may wait for it,
/// and whether to register it at once. A plain `&str` is a method registered at once with the core's default wait.
#[derive(Debug, Clone)]
pub struct Method {
    pub(crate) name: String,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) later: bool,
}

impl Method {
    /// A method registered once the handshake is done, with the core's default wait (30 seconds).
    pub fn new(name: &str) -> Method {
        Method {
            name: name.to_string(),
            timeout_ms: None,
            later: false,
        }
    }

    /// How long the core waits for an answer before giving the caller `method_timeout`: 1000 to 600000.
    pub fn timeout_ms(mut self, ms: u64) -> Method {
        self.timeout_ms = Some(ms);
        self
    }

    /// Don't register it with the others: it stays unregistered (callers get `unregistered`, the bus doesn't count it
    /// ready) until [`crate::Core::enable_methods`] names it, for example once a model has downloaded.
    pub fn later(mut self) -> Method {
        self.later = true;
        self
    }
}

impl From<&str> for Method {
    fn from(name: &str) -> Method {
        Method::new(name)
    }
}

/// A method's error: a message for people and, optionally, a code callers can tell apart (`bad_audio`, `mic_busy`).
/// It goes back as a JSON-RPC error `{"code": -32000, "message", "data": {"code"}}`; without a code, `-32603`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodError {
    /// The code callers match on.
    pub code: Option<String>,
    /// What went wrong, for people.
    pub message: String,
}

impl MethodError {
    /// An error with a code.
    pub fn new(code: &str, message: impl Into<String>) -> MethodError {
        MethodError {
            code: Some(code.to_string()),
            message: message.into(),
        }
    }

    pub(crate) fn into_json(self) -> Value {
        match self.code {
            Some(code) => json!({"code": CODED, "message": self.message, "data": {"code": code}}),
            None => json!({"code": INTERNAL, "message": self.message}),
        }
    }
}

impl From<String> for MethodError {
    fn from(message: String) -> MethodError {
        MethodError {
            code: None,
            message,
        }
    }
}

impl From<&str> for MethodError {
    fn from(message: &str) -> MethodError {
        MethodError::from(message.to_string())
    }
}

/// Every declared method and whether it is registered now.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    methods: BTreeMap<String, (Option<u64>, bool)>,
}

impl Registry {
    pub(crate) fn declare(&mut self, method: &Method) {
        self.methods
            .insert(method.name.clone(), (method.timeout_ms, !method.later));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// Turn the named ones on or off; names never declared are returned, nothing changes then.
    pub(crate) fn set(&mut self, names: &[&str], on: bool) -> Result<(), Vec<String>> {
        let unknown: Vec<String> = names
            .iter()
            .filter(|name| !self.methods.contains_key(**name))
            .map(|name| (*name).to_string())
            .collect();
        if !unknown.is_empty() {
            return Err(unknown);
        }
        for name in names {
            if let Some(entry) = self.methods.get_mut(*name) {
                entry.1 = on;
            }
        }
        Ok(())
    }

    /// The `package.methods` params: the ones on, by name.
    pub(crate) fn params(&self) -> Value {
        let methods: Vec<Value> = self
            .methods
            .iter()
            .filter(|(_, (_, on))| *on)
            .map(|(name, (timeout, _))| match timeout {
                Some(ms) => json!({"name": name, "timeout_ms": ms}),
                None => json!({"name": name}),
            })
            .collect();
        json!({"methods": methods})
    }
}

#[cfg(test)]
mod tests;
