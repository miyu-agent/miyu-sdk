//! What handlers get and give back: [`Tool`] (a tool's spec), [`Call`] (one `tool.call`), [`Reply`] (its result, with
//! [`Change`] for a checked file change), [`Channel`] (`miyu call` for the commands a tool runs), [`Core`] (calling the core,
//! reading config) and [`CoreError`] (a refusal).

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::method::Registry;
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

    /// Which parameters are file paths: `[{"parameter": "file_path", "write": true}]` (top-level string or string-array
    /// parameters; `write` needs `write` access). The core checks them like the built-in `edit`, hands the allowed
    /// locations over as [`Call::checked`], and only then keeps the file effects you report.
    pub fn paths(self, paths: Value) -> Tool {
        self.set("paths", paths)
    }

    /// Parameters that only make sense when the session can run things in the background (not required ones).
    pub fn background_parameters(self, names: &[&str]) -> Tool {
        self.set("background_parameters", json!(names))
    }

    /// The tool runs commands: the package needs the `tools.commands` capability, and [`Call::channel`] lets `miyu call`
    /// in those commands reach the session.
    pub fn runs_commands(self, runs: bool) -> Tool {
        self.set("runs_commands", json!(runs))
    }
}

/// The extra checks [`Reply::changed_with`] can carry: content hashes ([`crate::content_hash`]) or the texts before and
/// after. When one doesn't match what the core read, it drops the effect. `None` leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    /// The content hash before the call; a new file has none.
    pub before: Option<String>,
    /// The content hash after.
    pub after: Option<String>,
    /// The whole text before the call.
    pub before_text: Option<String>,
    /// The whole text after.
    pub after_text: Option<String>,
}

/// A tool's result. `blocks` are what the model sees; `error` marks a failure (explain it in the text); `data` goes only
/// to scripts calling the tool with `miyu call`; `detail` only to the interfaces; `sent` are images she sends;
/// `end_turn` ends the turn after this tool. `effects` say which files the call read, changed or trashed and the to-do
/// list it wrote: [`Reply::changed`], [`Reply::read`], [`Reply::trashed`], [`Reply::todos`].
///
/// ```
/// # use miyu_ext::Reply;
/// let reply = Reply::text("Edited a.txt.").changed("/home/me/src/a.txt");
/// ```
#[derive(Debug, Clone, Default)]
pub struct Reply {
    blocks: Vec<Value>,
    error: bool,
    data: Option<Value>,
    detail: Option<Value>,
    sent: Option<Value>,
    end_turn: bool,
    effects: Vec<Value>,
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

    /// Add effects as the protocol writes them; the helpers below write the four kinds the core takes.
    pub fn effects(mut self, effects: Vec<Value>) -> Reply {
        self.effects.extend(effects);
        self
    }

    /// The file at `path` was changed or created (`file.changed`). The core reads it itself, before the call and after
    /// your answer, so the path is enough. It must be one of [`Call::checked`] from a parameter declared in
    /// [`Tool::paths`] with `write`.
    pub fn changed(self, path: &str) -> Reply {
        self.changed_with(path, Change::default())
    }

    /// Same as [`Reply::changed`], with the extra checks in `change`.
    pub fn changed_with(mut self, path: &str, change: Change) -> Reply {
        let mut effect = json!({"kind": "file.changed", "path": path});
        for (key, value) in [
            ("before", change.before),
            ("after", change.after),
            ("before_text", change.before_text),
            ("after_text", change.after_text),
        ] {
            if let Some(value) = value {
                effect[key] = json!(value);
            }
        }
        self.effects.push(effect);
        self
    }

    /// The call read the file at `path` (`file.read`); `lines` is `[first, last]`, counting from 1. The path must be one
    /// of [`Call::checked`].
    pub fn read(mut self, path: &str, lines: Option<[u64; 2]>) -> Reply {
        let mut effect = json!({"kind": "file.read", "path": path});
        if let Some(lines) = lines {
            effect["lines"] = json!(lines);
        }
        self.effects.push(effect);
        self
    }

    /// The file at `path` went to the trash, now at `trash` (shown to people only, at most 4096 characters). The path
    /// must be one of [`Call::checked`], declared with `write`, and gone by the time you answer.
    pub fn trashed(mut self, path: &str, trash: &str) -> Reply {
        self.effects
            .push(json!({"kind": "file.trashed", "path": path, "trash": trash}));
        self
    }

    /// The call replaced the to-do list (`todo.written`, as the built-in `todowrite`): `todos` is
    /// `[{"content", "status"}]`, `done` the finished list it cleared. Not kept when the reply is a failure.
    pub fn todos(mut self, todos: Value, done: Option<Value>) -> Reply {
        let mut effect = json!({"kind": "todo.written", "todos": todos});
        if let Some(done) = done {
            effect["done"] = done;
        }
        self.effects.push(effect);
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
        if !self.effects.is_empty() {
            out["effects"] = Value::Array(self.effects);
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
    pub(crate) methods: Mutex<Registry>,
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

    /// Register the [later](crate::Method::later) methods named, with the ones already registered (`package.methods`
    /// replaces the whole list, so it is sent again in full). Call it once they can answer: a model downloaded, a key set.
    ///
    /// # Errors
    ///
    /// A name never declared with [`crate::Extension::method`] (nothing is sent then), or the core refused the list.
    pub fn enable_methods(&self, names: &[&str]) -> Result<(), CoreError> {
        self.switch_methods(names, true)
    }

    /// Unregister the methods named, keeping the rest: callers get `unregistered` and the bus stops counting them
    /// ready, until [`Core::enable_methods`] names them again.
    ///
    /// # Errors
    ///
    /// A name never declared (nothing is sent then), or the core refused the list.
    pub fn disable_methods(&self, names: &[&str]) -> Result<(), CoreError> {
        self.switch_methods(names, false)
    }

    fn switch_methods(&self, names: &[&str], on: bool) -> Result<(), CoreError> {
        // Held while sending, so two switches from different threads reach the core in the order they changed the list.
        let mut methods = self
            .shared
            .methods
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        methods.set(names, on).map_err(|unknown| CoreError {
            code: None,
            reason: Some("unknown_method".to_string()),
            message: format!("never declared: {}", unknown.join(", ")),
            data: json!({"reason": "unknown_method"}),
        })?;
        self.call("package.methods", methods.params()).map(drop)
    }

    /// Send the methods registered now.
    pub(crate) fn register_methods(&self) -> Result<(), CoreError> {
        let methods = self
            .shared
            .methods
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.call("package.methods", methods.params()).map(drop)
    }

    /// Say why on stderr and exit with code 1, which tells the core not to restart: for wrong settings and taken ports.
    pub fn fatal(&self, message: &str) -> ! {
        crate::extension::fatal(message)
    }
}

/// The `miyu call` channel of one call ([`Call::channel`]): set it into the commands you start for the call with
/// [`Call::command_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// The endpoint: a socket path on Unix, a named pipe on Windows. `MIYU_CALL_SOCK`.
    pub sock: String,
    /// The token. `MIYU_CALL_TOKEN`.
    pub token: String,
    /// A folder with only `miyu` in it, to put first on `PATH`; `None` when the core couldn't make it.
    pub bin: Option<String>,
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

    /// For tools that declare [`Tool::paths`]: the real locations the core's permission check allowed, in the order the
    /// paths appear in the arguments. Write only these: a link on the path may have been swapped since the check. Empty
    /// for other tools.
    pub fn checked(&self) -> Vec<&str> {
        self.params["checked"]
            .as_array()
            .map(|checked| checked.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// For tools that declare [`Tool::runs_commands`], when the session has a channel: where `miyu call` in the commands
    /// you start reaches this session.
    pub fn channel(&self) -> Option<Channel> {
        let channel = &self.params["channel"];
        Some(Channel {
            sock: channel["sock"].as_str()?.to_string(),
            token: channel["token"].as_str()?.to_string(),
            bin: channel["bin"].as_str().map(String::from),
        })
    }

    /// The environment to add to a command you start for this call: `MIYU_CALL_SOCK`, `MIYU_CALL_TOKEN` and `PATH` with
    /// `bin` first, so `miyu call` in it reaches this session; empty without a channel. Start the command yourself (a
    /// child or grandchild of this process) and answer after it ends: the channel closes with the answer.
    ///
    /// ```no_run
    /// # fn run(call: &miyu_ext::Call) -> std::io::Result<std::process::Output> {
    /// std::process::Command::new("sh").args(["-c", "miyu call memory_search --query x"]).envs(call.command_env()).output()
    /// # }
    /// ```
    pub fn command_env(&self) -> Vec<(String, OsString)> {
        let Some(channel) = self.channel() else {
            return Vec::new();
        };
        let mut env = vec![
            ("MIYU_CALL_SOCK".to_string(), OsString::from(channel.sock)),
            ("MIYU_CALL_TOKEN".to_string(), OsString::from(channel.token)),
        ];
        if let Some(bin) = channel.bin.filter(|bin| !bin.is_empty()) {
            let mut path = OsString::from(bin);
            if let Some(old) = std::env::var_os("PATH").filter(|old| !old.is_empty()) {
                path.push(if cfg!(windows) { ";" } else { ":" });
                path.push(old);
            }
            env.push(("PATH".to_string(), path));
        }
        env
    }
}
