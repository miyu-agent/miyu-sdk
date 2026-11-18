//! [`Extension`]: register tools, commands, methods and handlers, then [`Extension::run`]. Each request from the core is
//! answered on its own thread, so a slow tool never holds up the others; answers go back by id.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use serde_json::{Map, Value, json};

use crate::method::{Method, MethodError, Registry};
use crate::reply::{Call, CallLanguage, Core, Reply, Shared, Tool};
use crate::wire::{Wire, not_found};
use crate::{EXIT_CONFIG, PROTOCOL};

type ToolFn = Box<dyn Fn(&Value, &Call) -> Reply + Send + Sync>;
type CommandFn = Box<dyn Fn(&Value, &Core) -> String + Send + Sync>;
type MethodFn =
    Box<dyn Fn(&Value, Option<&str>, &Core) -> Result<Value, MethodError> + Send + Sync>;
type OnFn = Box<dyn Fn(&Value, &Core) -> Result<Value, MethodError> + Send + Sync>;
type ConfigFn = Box<dyn Fn(&Map<String, Value>, &Core) + Send + Sync>;
type ReadyFn = Box<dyn Fn(&Core) + Send + Sync>;

/// Everything registered, shared by the answering threads.
#[derive(Default)]
struct Handlers {
    tools: HashMap<String, ToolFn>,
    commands: HashMap<String, CommandFn>,
    methods: HashMap<String, MethodFn>,
    on: HashMap<String, OnFn>,
    config: Vec<ConfigFn>,
    ready: Vec<ReadyFn>,
}

/// An extension. `kind` and `version` go into the handshake (`head`), for the core's log.
pub struct Extension {
    kind: String,
    version: String,
    specs: Vec<Value>,
    handlers: Handlers,
    registry: Registry,
}

impl Extension {
    /// A new extension with nothing registered.
    pub fn new(kind: &str, version: &str) -> Extension {
        Extension {
            kind: kind.to_string(),
            version: version.to_string(),
            specs: Vec::new(),
            handlers: Handlers::default(),
            registry: Registry::default(),
        }
    }

    /// Register a tool: `handler` gets the arguments and the [`Call`] and returns a [`Reply`].
    pub fn tool(
        &mut self,
        tool: Tool,
        handler: impl Fn(&Value, &Call) -> Reply + Send + Sync + 'static,
    ) -> &mut Extension {
        let name = tool.spec["name"].as_str().unwrap_or_default().to_string();
        self.specs.push(tool.spec);
        self.handlers.tools.insert(name, Box::new(handler));
        self
    }

    /// Handle the slash command `name` (the manifest's `[[slash]]`): `handler` gets the `command.call` params and returns
    /// the text shown to the person.
    pub fn command(
        &mut self,
        name: &str,
        handler: impl Fn(&Value, &Core) -> String + Send + Sync + 'static,
    ) -> &mut Extension {
        self.handlers
            .commands
            .insert(name.to_string(), Box::new(handler));
        self
    }

    /// Handle the method `name` called by your page, a widget button, a head or another package: `handler` gets the
    /// params and the calling package's id (`None` for a person); an `Err` goes back as a JSON-RPC error. Registered once
    /// the handshake is done, with the core's default wait; for a wait, a coded error or later registration use
    /// [`Extension::method_with`].
    pub fn method(
        &mut self,
        name: &str,
        handler: impl Fn(&Value, Option<&str>, &Core) -> Result<Value, String> + Send + Sync + 'static,
    ) -> &mut Extension {
        self.method_with(Method::new(name), move |params, from, core| {
            handler(params, from, core).map_err(MethodError::from)
        })
    }

    /// Same as [`Extension::method`] for a [`Method`] (a bus name's methods are written in full, `stt.transcribe`): with
    /// how long the core waits, maybe registered [later](Method::later); an `Err` with a [`MethodError::code`] goes back
    /// with the code in the error's `data.code` (and `data.reason`), so callers can tell `bad_audio` from `mic_busy`.
    pub fn method_with(
        &mut self,
        method: Method,
        handler: impl Fn(&Value, Option<&str>, &Core) -> Result<Value, MethodError>
        + Send
        + Sync
        + 'static,
    ) -> &mut Extension {
        self.registry.declare(&method);
        self.handlers.methods.insert(method.name, Box::new(handler));
        self
    }

    /// Handle any other request or notification from the core (`user.append`, `search.query`, `widget.watch`...): for a
    /// request, what `handler` returns is the answer. Notifications are handled on the same ordered thread as config pushes.
    /// To answer with an error, use [`Extension::on_with`].
    pub fn on(
        &mut self,
        method: &str,
        handler: impl Fn(&Value, &Core) -> Value + Send + Sync + 'static,
    ) -> &mut Extension {
        self.on_with(method, move |params, core| Ok(handler(params, core)))
    }

    /// Same as [`Extension::on`], answering a request with an error when `handler` returns `Err`: a
    /// [`MethodError::code`] goes into the error's `data` as both `code` and `reason`, so the core can read it
    /// (`search.query` rate limited answers `MethodError::new("limited", …)`). For a notification an `Err` only goes to
    /// stderr.
    pub fn on_with(
        &mut self,
        method: &str,
        handler: impl Fn(&Value, &Core) -> Result<Value, MethodError> + Send + Sync + 'static,
    ) -> &mut Extension {
        self.handlers
            .on
            .insert(method.to_string(), Box::new(handler));
        self
    }

    /// Call `handler` after a config push with the changed keys (`null` when unset). Handlers run on one thread of their own,
    /// in the order the pushes came, so they may call the core; [`Core::config`] already has the new values when a request
    /// after the push arrives.
    pub fn on_config(
        &mut self,
        handler: impl Fn(&Map<String, Value>, &Core) + Send + Sync + 'static,
    ) -> &mut Extension {
        self.handlers.config.push(Box::new(handler));
        self
    }

    /// Call `handler` on its own thread once the handshake and registration are done.
    pub fn ready(&mut self, handler: impl Fn(&Core) + Send + Sync + 'static) -> &mut Extension {
        self.handlers.ready.push(Box::new(handler));
        self
    }

    /// Whether any method is registered: then registration ends with `package.methods`, not `provide`.
    pub(crate) fn has_methods(&self) -> bool {
        !self.registry.is_empty()
    }

    /// Serve on stdin and stdout until stdin ends (the core asking you to stop). A refused handshake or a protocol
    /// mismatch is said on stderr and exits with code 1, so the core doesn't restart.
    pub fn run(self) {
        let stdin = std::io::stdin();
        if let Err(why) = self.run_with(stdin.lock(), Box::new(std::io::stdout())) {
            fatal(&why);
        }
    }

    /// Same as [`Extension::run`] on other streams (tests use pipes, see [`crate::FakeCore`]).
    ///
    /// # Errors
    ///
    /// The core refused the handshake or chose a protocol version this crate doesn't speak.
    pub fn run_with(
        self,
        input: impl BufRead,
        output: Box<dyn Write + Send>,
    ) -> Result<(), String> {
        let core = Core {
            shared: Arc::new(Shared {
                wire: Wire::new(output),
                config: Mutex::new(Map::new()),
                language: Mutex::new("en".to_string()),
                account: Mutex::new(None),
                methods: Mutex::new(self.registry),
            }),
        };
        let mut hello =
            json!({"protocol": PROTOCOL, "head": {"kind": self.kind, "version": self.version}});
        if let Some(locale) = locale_from(&|name| std::env::var(name).ok()) {
            hello["locale"] = locale.into();
        }
        core.shared
            .wire
            .send(&json!({"jsonrpc": "2.0", "id": "hello", "method": "hello", "params": hello}));
        let server = Arc::new(Server {
            handlers: self.handlers,
            calls: Mutex::new(HashMap::new()),
            core: core.clone(),
            notices: Mutex::new(None),
        });
        let mut specs = self.specs;
        let mut shaken = false;
        for line in input.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                eprintln!(
                    "miyu-ext: unreadable line from the core: {}",
                    line.chars().take(200).collect::<String>()
                );
                continue;
            };
            if !shaken {
                // Nothing else should come before the handshake answer; skip it if it does.
                if message["id"] != "hello" {
                    continue;
                }
                shake(&core, &message)?;
                shaken = true;
                let server = Arc::clone(&server);
                let specs = std::mem::take(&mut specs);
                std::thread::spawn(move || server.register(specs));
                continue;
            }
            server.dispatch(message);
        }
        Ok(())
    }
}

/// Read the handshake answer: config, language, account; refuse a protocol we don't speak.
fn shake(core: &Core, message: &Value) -> Result<(), String> {
    if let Some(error) = message.get("error") {
        let error = crate::CoreError::from_json(error);
        if error.reason.as_deref() == Some("protocol_mismatch") {
            return Err(format!(
                "miyu-ext {} speaks protocol {}-{}, which this Miyu doesn't: use a matching miyu-ext ({})",
                env!("CARGO_PKG_VERSION"),
                PROTOCOL[0],
                PROTOCOL[1],
                error.message
            ));
        }
        return Err(format!("the handshake was refused: {error}"));
    }
    let result = &message["result"];
    let chosen = result["protocol"].as_u64();
    if !chosen.is_some_and(|major| (PROTOCOL[0]..=PROTOCOL[1]).contains(&major)) {
        return Err(format!(
            "miyu-ext {} speaks protocol {}-{}, the core chose {}",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL[0],
            PROTOCOL[1],
            result["protocol"]
        ));
    }
    let shared = &core.shared;
    if let Some(config) = result["config"].as_object() {
        *shared.config.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
    }
    if let Some(language) = result["language"].as_str() {
        *shared
            .language
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = language.to_string();
    }
    *shared
        .account
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = result["account"].as_str().map(String::from);
    Ok(())
}

/// The system language to tell the core at the handshake (`locale` in `hello`): the first of `LC_ALL`, `LC_MESSAGES`,
/// `LANG` that is set and not empty, the way a terminal picks its language. With `ui.language` on `auto` the core shows
/// people-facing text in this language; without it the core falls back to English. `env` reads one variable.
#[must_use]
pub(crate) fn locale_from(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(env)
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Say why and exit with code 1 (restarting won't help).
pub(crate) fn fatal(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(EXIT_CONFIG)
}

/// A call in flight: its session and call id. The core numbers calls per session, so two sessions can run the same
/// call id at once.
type CallKey = (String, String);

/// The key of a `tool.call` or `tool.cancel`.
fn call_key(params: &Value) -> CallKey {
    let field = |name: &str| params[name].as_str().unwrap_or_default().to_string();
    (field("session"), field("call_id"))
}

/// The running side: handlers, the calls in flight (for `tool.cancel`) and the core.
struct Server {
    handlers: Handlers,
    calls: Mutex<HashMap<CallKey, Vec<Arc<AtomicBool>>>>,
    core: Core,
    /// Config pushes and other notifications go to one thread, in order: a handler may call the core and wait, which
    /// it couldn't on the reading thread (the answer would never be read).
    notices: Mutex<Option<mpsc::Sender<Notice>>>,
}

/// A notification for the notice thread.
enum Notice {
    Config(Map<String, Value>),
    Other(String, Value),
}

impl Server {
    /// `provide` (even with no tools: commands and search need it once), `package.methods`, then the ready handlers.
    fn register(self: Arc<Server>, specs: Vec<Value>) {
        if let Err(error) = self.core.call("provide", json!({"tools": specs})) {
            eprintln!("miyu-ext: provide failed: {error}");
            return;
        }
        if !self.handlers.methods.is_empty()
            && let Err(error) = self.core.register_methods()
        {
            eprintln!("miyu-ext: package.methods failed: {error}");
            return;
        }
        for index in 0..self.handlers.ready.len() {
            let server = Arc::clone(&self);
            std::thread::spawn(move || (server.handlers.ready[index])(&server.core));
        }
    }

    fn dispatch(self: &Arc<Server>, message: Value) {
        let Some(method) = message["method"].as_str().map(String::from) else {
            let answer = match message.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(message["result"].clone()),
            };
            self.core
                .shared
                .wire
                .deliver(message["id"].as_str().unwrap_or_default(), answer);
            return;
        };
        let params = message["params"].clone();
        let Some(id) = message.get("id").cloned() else {
            self.notified(&method, &params);
            return;
        };
        // Register the cancel flag before spawning: `tool.cancel` may arrive before the answering thread starts.
        let cancelled = (method == "tool.call").then(|| {
            let flag = Arc::new(AtomicBool::new(false));
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(call_key(&params))
                .or_default()
                .push(Arc::clone(&flag));
            flag
        });
        let mine = cancelled.clone();
        let server = Arc::clone(self);
        std::thread::spawn(move || {
            let answer = catch_unwind(AssertUnwindSafe(|| {
                server.answer(&method, &params, cancelled)
            }))
            .unwrap_or_else(|_| match method.as_str() {
                "tool.call" => Ok(Reply::failed("the tool panicked").into_json()),
                _ => Err(json!({"code": -32603, "message": format!("{method} panicked")})),
            });
            if let Some(mine) = mine {
                // Only this call leaves: another one under the same key stays cancellable.
                let key = call_key(&params);
                let mut calls = server.calls.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(running) = calls.get_mut(&key) {
                    running.retain(|one| !Arc::ptr_eq(one, &mine));
                    if running.is_empty() {
                        calls.remove(&key);
                    }
                }
            }
            server.core.shared.wire.answer(&id, answer);
        });
    }

    /// Hand a notification to the notice thread, starting it the first time.
    fn queue(self: &Arc<Server>, notice: Notice) {
        let mut sender = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
        let sender = sender.get_or_insert_with(|| {
            let (sender, received) = mpsc::channel::<Notice>();
            let server = Arc::clone(self);
            std::thread::spawn(move || {
                for notice in received {
                    match notice {
                        Notice::Config(changed) => {
                            for handler in &server.handlers.config {
                                handler(&changed, &server.core);
                            }
                        }
                        Notice::Other(method, params) => {
                            if let Some(handler) = server.handlers.on.get(&method)
                                && let Err(error) = handler(&params, &server.core)
                            {
                                eprintln!("miyu-ext: {method}: {}", error.message);
                            }
                        }
                    }
                }
            });
            sender
        });
        // The thread only ends with the process.
        sender.send(notice).unwrap_or_default();
    }

    fn notified(self: &Arc<Server>, method: &str, params: &Value) {
        match method {
            "extension.config" => {
                let changed = params["keys"].as_object().cloned().unwrap_or_default();
                {
                    let mut config = self
                        .core
                        .shared
                        .config
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    for (key, value) in &changed {
                        match value {
                            Value::Null => config.remove(key),
                            value => config.insert(key.clone(), value.clone()),
                        };
                    }
                }
                if !self.handlers.config.is_empty() {
                    self.queue(Notice::Config(changed));
                }
            }
            "tool.cancel" => {
                let calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
                for cancelled in calls.get(&call_key(params)).into_iter().flatten() {
                    cancelled.store(true, Ordering::Relaxed);
                }
            }
            _ => {
                if self.handlers.on.contains_key(method) {
                    self.queue(Notice::Other(method.to_string(), params.clone()));
                }
            }
        }
    }

    fn answer(
        &self,
        method: &str,
        params: &Value,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Value, Value> {
        match method {
            "tool.call" => {
                let tool = params["tool"].as_str().unwrap_or_default();
                let handler = self
                    .handlers
                    .tools
                    .get(tool)
                    .ok_or_else(|| not_found(method))?;
                let call = Call::new(
                    params.clone(),
                    self.core.clone(),
                    cancelled.unwrap_or_default(),
                );
                Ok(handler(&params["args"], &call).into_json())
            }
            "command.call" => {
                let command = params["command"].as_str().unwrap_or_default();
                let handler = self
                    .handlers
                    .commands
                    .get(command)
                    .ok_or_else(|| not_found(method))?;
                let _language = CallLanguage::set(params["language"].as_str());
                Ok(json!({"said": handler(params, &self.core)}))
            }
            "method.call" => {
                let name = params["method"].as_str().unwrap_or_default();
                let handler = self
                    .handlers
                    .methods
                    .get(name)
                    .ok_or_else(|| not_found(method))?;
                let _language = CallLanguage::set(params["language"].as_str());
                handler(&params["params"], params["from"].as_str(), &self.core)
                    .map_err(MethodError::into_json)
            }
            _ => {
                let handler = self
                    .handlers
                    .on
                    .get(method)
                    .ok_or_else(|| not_found(method))?;
                handler(params, &self.core).map_err(MethodError::into_json)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::locale_from;

    /// `LC_ALL` wins over `LC_MESSAGES` over `LANG`; empty ones are skipped; none at all sends nothing.
    #[test]
    fn locale_follows_the_terminal_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        let all = env(&[
            ("LC_ALL", "ja_JP.UTF-8"),
            ("LC_MESSAGES", "en_US"),
            ("LANG", "zh_CN.UTF-8"),
        ]);
        assert_eq!(locale_from(&all).as_deref(), Some("ja_JP.UTF-8"));
        let messages = env(&[
            ("LC_ALL", " "),
            ("LC_MESSAGES", "en_US"),
            ("LANG", "zh_CN.UTF-8"),
        ]);
        assert_eq!(locale_from(&messages).as_deref(), Some("en_US"));
        let lang = env(&[("LANG", "zh_CN.UTF-8")]);
        assert_eq!(locale_from(&lang).as_deref(), Some("zh_CN.UTF-8"));
        assert_eq!(locale_from(&env(&[])), None);
    }
}
