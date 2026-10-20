//! What handlers get and give back: [`Tool`] (a tool's spec), [`Call`] (one `tool.call`), [`Reply`] (its result),
//! [`Core`] (calling the core, reading config) and [`CoreError`] (a refusal).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::wire::Wire;

/// A tool's spec, as `provide` sends it. `description` and the schema are for the model: short English sentences, at
/// most three. Set `access` honestly: `read`, `write`, `execute`, `network`, `outbound` or `venue`.
#[derive(Debug, Clone)]
pub struct Tool {
    pub(crate) spec: Value,
}

impl Tool {
    /// A tool named `name`: no parameters, `read` access, local sessions only, until set otherwise.
    pub fn new(name: &str, description: &str) -> Tool {
        Tool {
            spec: json!({
                "name": name,
                "description": description,
                "input_schema": {"type": "object", "properties": {}},
                "access": "read",
                "venues": ["local"],
            }),
        }
    }

    fn set(mut self, key: &str, value: Value) -> Tool {
        self.spec[key] = value;
        self
    }

    /// The parameters' JSON Schema; it must be an object.
    pub fn schema(self, schema: Value) -> Tool {
        self.set("input_schema", schema)
    }

    /// What it touches: `read`, `write`, `execute`, `network`, `outbound` or `venue`.
    pub fn access(self, access: &str) -> Tool {
        self.set("access", json!(access))
    }

    /// Which sessions get it: `local`, `private`, `group`.
    pub fn venues(self, venues: &[&str]) -> Tool {
        self.set("venues", json!(venues))
    }

    /// How long a call may take, 1000 to 600000 milliseconds.
    pub fn timeout_ms(self, ms: u64) -> Tool {
        self.set("timeout_ms", json!(ms))
    }

    /// The name people see, per language: `{"en": "Greet", "zh": "打招呼"}`.
    pub fn display_name(self, names: Value) -> Tool {
        self.set("display_name", names)
    }

    /// Which argument the timeline shows after the name.
    pub fn subject(self, argument: &str) -> Tool {
        self.set("subject", json!(argument))
    }

    /// The model sees only the name and one line until it loads the tool.
    pub fn need_load(self, need: bool) -> Tool {
        self.set("need_load", json!(need))
    }

    /// Scripts may call it with `miyu call` (read, network and execute tools only).
    pub fn clicall(self, open: bool) -> Tool {
        self.set("clicall", json!(open))
    }

    /// The manifest feature it belongs to, for packages with several.
    pub fn feature(self, feature: &str) -> Tool {
        self.set("feature", json!(feature))
    }
}

/// A tool's result. `blocks` are what the model sees; `error` marks a failure (explain it in the text); `data` goes only
/// to scripts calling the tool with `miyu call`; `detail` only to the interfaces; `sent` are images she sends;
/// `end_turn` ends the turn after this tool.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    blocks: Vec<Value>,
    error: bool,
    data: Option<Value>,
    detail: Option<Value>,
    sent: Option<Value>,
    end_turn: bool,
}

impl Reply {
    /// One block of text.
    pub fn text(text: impl Into<String>) -> Reply {
        Reply::blocks(vec![json!({"type": "text", "text": text.into()})])
    }

    /// Content blocks as the protocol writes them (text, images after `blob.put`).
    pub fn blocks(blocks: Vec<Value>) -> Reply {
        Reply {
            blocks,
            ..Reply::default()
        }
    }

    /// A failure, explained in `text` for the model.
    pub fn failed(text: impl Into<String>) -> Reply {
        Reply {
            error: true,
            ..Reply::text(text)
        }
    }

    /// Structured data for scripts (`miyu call`); not shown to the model.
    pub fn data(mut self, data: Value) -> Reply {
        self.data = Some(data);
        self
    }

    /// A card for the interfaces (links, table or images shape); not shown to the model.
    pub fn detail(mut self, detail: Value) -> Reply {
        self.detail = Some(detail);
        self
    }

    /// Images she sends (stickers): at most four.
    pub fn sent(mut self, sent: Value) -> Reply {
        self.sent = Some(sent);
        self
    }

    /// End the turn after this tool.
    pub fn end_turn(mut self) -> Reply {
        self.end_turn = true;
        self
    }

    pub(crate) fn into_json(self) -> Value {
        let mut out = json!({"blocks": self.blocks});
        for (key, value) in [
            ("data", self.data),
            ("detail", self.detail),
            ("sent", self.sent),
        ] {
            if let Some(value) = value {
                out[key] = value;
            }
        }
        if self.error {
            out["error"] = json!(true);
        }
        if self.end_turn {
            out["end_turn"] = json!(true);
        }
        out
    }
}

impl From<String> for Reply {
    fn from(text: String) -> Reply {
        Reply::text(text)
    }
}

impl From<&str> for Reply {
    fn from(text: &str) -> Reply {
        Reply::text(text)
    }
}

/// The core refused a request: `reason` is the stable code (`data.reason`), `message` is for people.
#[derive(Debug, Clone)]
pub struct CoreError {
    /// The JSON-RPC code.
    pub code: Option<i64>,
    /// The stable reason code, such as `needs_approval`.
    pub reason: Option<String>,
    /// What to tell people.
    pub message: String,
    /// The whole `data`.
    pub data: Value,
}

impl CoreError {
    pub(crate) fn from_json(error: &Value) -> CoreError {
        CoreError {
            code: error.get("code").and_then(Value::as_i64),
            reason: error["data"]["reason"].as_str().map(String::from),
            message: error["message"].as_str().unwrap_or_default().to_string(),
            data: error.get("data").cloned().unwrap_or(Value::Null),
        }
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self
            .reason
            .clone()
            .or_else(|| self.code.map(|code| code.to_string()));
        write!(f, "{}: {}", code.unwrap_or_default(), self.message)
    }
}

impl std::error::Error for CoreError {}

/// What the handshake told us, shared by every handler.
pub(crate) struct Shared {
    pub(crate) wire: Wire,
    pub(crate) config: Mutex<Map<String, Value>>,
    pub(crate) language: Mutex<String>,
    pub(crate) account: Mutex<Option<String>>,
}

/// The core, from a handler: call its methods, read this package's config.
#[derive(Clone)]
pub struct Core {
    pub(crate) shared: Arc<Shared>,
}

impl Core {
    /// Call a core method and wait up to 90 seconds.
    ///
    /// # Errors
    ///
    /// The core refused it, or no answer came in time.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, CoreError> {
        self.call_within(method, params, Duration::from_secs(90))
    }

    /// Same as [`Core::call`], waiting up to `wait`.
    ///
    /// # Errors
    ///
    /// The core refused it, or no answer came in time.
    pub fn call_within(
        &self,
        method: &str,
        params: Value,
        wait: Duration,
    ) -> Result<Value, CoreError> {
        self.shared
            .wire
            .call(method, &params, wait)
            .map_err(|error| CoreError::from_json(&error))
    }

    /// This package's settings now (secrets as their values), as of the handshake and the pushes since.
    pub fn config(&self) -> Map<String, Value> {
        self.shared
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The language for people: `zh`, `en` or `ja`.
    pub fn language(&self) -> String {
        self.shared
            .language
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Who this extension is: its system account, or the admin.
    pub fn account(&self) -> Option<String> {
        self.shared
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Say why on stderr and exit with code 1, which tells the core not to restart: for wrong settings and taken ports.
    pub fn fatal(&self, message: &str) -> ! {
        crate::extension::fatal(message)
    }
}

/// One `tool.call`: the arguments are already repaired to the schema. Check [`Call::cancelled`] in long work: it turns
/// true when the call timed out or the person interrupted.
pub struct Call {
    params: Value,
    cancelled: Arc<AtomicBool>,
    core: Core,
}

impl Call {
    pub(crate) fn new(params: Value, core: Core, cancelled: Arc<AtomicBool>) -> Call {
        Call {
            params,
            cancelled,
            core,
        }
    }

    /// The session it came from.
    pub fn session(&self) -> &str {
        self.params["session"].as_str().unwrap_or_default()
    }

    /// The core's id for this call.
    pub fn call_id(&self) -> &str {
        self.params["call_id"].as_str().unwrap_or_default()
    }

    /// The session's working directory.
    pub fn cwd(&self) -> &str {
        self.params["cwd"].as_str().unwrap_or_default()
    }

    /// Whether the core counts the person who asked as the owner.
    pub fn owner(&self) -> bool {
        self.params["owner"].as_bool().unwrap_or(false)
    }

    /// Who asked, as the protocol writes it.
    pub fn by(&self) -> &Value {
        &self.params["by"]
    }

    /// Whether the call timed out or was interrupted.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// The core, to call its methods or read config.
    pub fn core(&self) -> &Core {
        &self.core
    }
}
