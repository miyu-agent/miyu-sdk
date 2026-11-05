//! [`FakeCore`]: a core in the same process, for testing an extension without Miyu. It shakes hands (handing over the
//! config), answers `provide`, `package.methods` and whatever you add with [`FakeCore::answer`], and lets you call tools,
//! commands and methods and push notifications. Dropping it (or [`FakeCore::close`]) ends the extension's stdin.
//!
//! Like the core, it checks what `provide` declares (`paths`, `background_parameters`, `runs_commands`): a wrong one is
//! refused with `bad_tool`, and a tool that runs commands without `tools.commands` approved
//! ([`FakeCore::start_approved`]) with `needs_approval`; the refusal is in [`FakeCore::refused`]. [`FakeCore::call_tool`]
//! sends `checked` (the declared paths resolved against [`FakeCore::set_cwd`] and [`FakeCore::set_home`], following
//! links) and `channel` (a fake one, [`FakeCore::set_channel`]; the fake core doesn't serve it) the way the core does,
//! then vets the reply's `effects` as the core does: the ones it keeps are in [`FakeCore::effects`] (paths as real
//! locations), the ones it drops in [`FakeCore::dropped`] as `{"tool", "kind", "why"}`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::Extension;
use crate::effects::{self, Context, Place};

type AnswerFn = Box<dyn Fn(&Value) -> Result<Value, Value> + Send + Sync>;

/// What the reading thread shares with the test.
struct Seen {
    received: Mutex<Vec<Value>>,
    waiting: Mutex<HashMap<String, Sender<Result<Value, Value>>>>,
    answers: Mutex<HashMap<String, AnswerFn>>,
    tools: Mutex<Vec<Value>>,
    methods: Mutex<Vec<Value>>,
    registered: Mutex<Option<Sender<()>>>,
    expects_methods: bool,
    hello: Value,
    /// The approved capabilities; `None` approves every one.
    approved: Option<Vec<String>>,
    refused: Mutex<Option<Value>>,
}

/// A core in-process; see the module docs.
pub struct FakeCore {
    seen: Arc<Seen>,
    writer: Arc<Mutex<Option<PipeWriter>>>,
    extension: Option<JoinHandle<Result<(), String>>>,
    next: AtomicU64,
    place: Mutex<Place>,
    channel: Mutex<Option<Value>>,
    effects: Mutex<Vec<Value>>,
    dropped: Mutex<Vec<Value>>,
}

impl FakeCore {
    /// Start `ext` against a fake core that hands over `config`, with every capability approved, and wait (up to ten
    /// seconds) until it has registered.
    ///
    /// # Panics
    ///
    /// The pipes can't be made, or the extension didn't shake hands and register in time.
    pub fn start(ext: Extension, config: Map<String, Value>) -> FakeCore {
        FakeCore::launch(ext, config, None)
    }

    /// Same as [`FakeCore::start`], with only the capabilities in `approved` approved (`tools.commands` matters here).
    ///
    /// # Panics
    ///
    /// The pipes can't be made, or the extension didn't shake hands and register in time.
    pub fn start_approved(
        ext: Extension,
        config: Map<String, Value>,
        approved: &[&str],
    ) -> FakeCore {
        let approved = approved.iter().map(|one| (*one).to_string()).collect();
        FakeCore::launch(ext, config, Some(approved))
    }

    fn launch(
        ext: Extension,
        config: Map<String, Value>,
        approved: Option<Vec<String>>,
    ) -> FakeCore {
        let expects_methods = ext.has_methods();
        let (into_ext, to_ext) = std::io::pipe().expect("a pipe to the extension");
        let (from_ext, out_of_ext) = std::io::pipe().expect("a pipe from the extension");
        let (registered, done) = mpsc::channel();
        let seen = Arc::new(Seen {
            received: Mutex::new(Vec::new()),
            waiting: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            tools: Mutex::new(Vec::new()),
            methods: Mutex::new(Vec::new()),
            registered: Mutex::new(Some(registered)),
            expects_methods,
            hello: json!({"protocol": crate::PROTOCOL[0], "config": config, "language": "en", "account": "admin",
                          "core": {"version": "0.0.0"}}),
            approved,
            refused: Mutex::new(None),
        });
        let writer = Arc::new(Mutex::new(Some(to_ext)));
        let extension = std::thread::spawn(move || {
            ext.run_with(BufReader::new(into_ext), Box::new(out_of_ext))
        });
        let (reading, written) = (Arc::clone(&seen), Arc::clone(&writer));
        std::thread::spawn(move || read_all(&reading, &written, BufReader::new(from_ext)));
        done.recv_timeout(Duration::from_secs(10))
            .expect("the extension shook hands and registered within ten seconds");
        let temp = std::env::temp_dir();
        let channel = json!({
            "sock": temp.join("miyu-call-fake.sock").to_string_lossy(),
            "token": "0".repeat(32),
            "bin": temp.join("miyu-call-fake-bin").to_string_lossy(),
        });
        let home =
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        FakeCore {
            seen,
            writer,
            extension: Some(extension),
            next: AtomicU64::new(0),
            place: Mutex::new(Place {
                cwd: PathBuf::from("/"),
                home,
            }),
            channel: Mutex::new(Some(channel)),
            effects: Mutex::new(Vec::new()),
            dropped: Mutex::new(Vec::new()),
        }
    }

    /// The working directory of the calls from now on (`/` at first): `cwd` in `tool.call`, and where relative paths
    /// are resolved from.
    pub fn set_cwd(&self, cwd: &Path) {
        lock(&self.place).cwd = cwd.to_path_buf();
    }

    /// The home folder `~` paths are resolved against (at first `HOME`, or `USERPROFILE` on Windows).
    pub fn set_home(&self, home: &Path) {
        lock(&self.place).home = Some(home.to_path_buf());
    }

    /// The `channel` a tool that runs commands gets (at first a fake one under the temp folder; the fake core doesn't
    /// serve it), or `None` for no channel.
    pub fn set_channel(&self, channel: Option<Value>) {
        *lock(&self.channel) = channel;
    }

    /// The `channel` the next calls get.
    pub fn channel(&self) -> Option<Value> {
        lock(&self.channel).clone()
    }

    /// The error `provide` was refused with (`bad_tool`, `needs_approval`), if it was.
    pub fn refused(&self) -> Option<Value> {
        lock(&self.seen.refused).clone()
    }

    /// The effects the core would have kept, from every call so far, paths as real locations.
    pub fn effects(&self) -> Vec<Value> {
        lock(&self.effects).clone()
    }

    /// The effects the core would have dropped, from every call so far: `{"tool", "kind", "why"}`, `why` being
    /// `not_checked`, `unchanged`, `mismatch`, `too_big`, `kind` or `shape`.
    pub fn dropped(&self) -> Vec<Value> {
        lock(&self.dropped).clone()
    }

    /// Forget the effects kept and dropped so far.
    pub fn clear_effects(&self) {
        lock(&self.effects).clear();
        lock(&self.dropped).clear();
    }

    /// Answer the extension's calls to `method` with `answer` (an `Err` goes back as the JSON-RPC error).
    pub fn answer(
        &self,
        method: &str,
        answer: impl Fn(&Value) -> Result<Value, Value> + Send + Sync + 'static,
    ) {
        self.seen
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(method.to_string(), Box::new(answer));
    }

    /// The tools `provide` registered.
    pub fn tools(&self) -> Vec<Value> {
        self.seen
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The methods `package.methods` registered.
    pub fn methods(&self) -> Vec<Value> {
        self.seen
            .methods
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every line the extension wrote, in order.
    pub fn received(&self) -> Vec<Value> {
        self.seen
            .received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Call `tool` the way the core does; the result as the extension wrote it, or `{"error": …}`. The effects the core
    /// would keep land in [`FakeCore::effects`], the rest in [`FakeCore::dropped`].
    pub fn call_tool(&self, tool: &str, args: Value) -> Value {
        let call_id = format!("c{}", self.next.load(Ordering::Relaxed) + 1);
        self.call_tool_as(tool, args, &call_id)
    }

    /// Same as [`FakeCore::call_tool`] with a chosen `call_id` (to cancel it).
    ///
    /// # Panics
    ///
    /// No answer in ten seconds.
    pub fn call_tool_as(&self, tool: &str, args: Value, call_id: &str) -> Value {
        let spec = self
            .tools()
            .into_iter()
            .find(|spec| spec["name"] == tool)
            .unwrap_or(Value::Null);
        // A copy: holding the lock through the call would hold up calls made side by side.
        let place = lock(&self.place).clone();
        let cwd = place.cwd.to_string_lossy().into_owned();
        let mut params = json!({"session": "s1", "call_id": call_id, "tool": tool, "args": args, "cwd": cwd, "owner": true});
        let judged = match spec.get("paths").filter(|paths| !paths.is_null()) {
            Some(paths) => {
                let judged = effects::judge(paths, &params["args"], &place);
                params["checked"] = judged
                    .iter()
                    .map(|one| json!(one.real.to_string_lossy()))
                    .collect();
                judged
            }
            None => Vec::new(),
        };
        if spec["runs_commands"] == true
            && let Some(channel) = self.channel()
        {
            params["channel"] = channel;
        }
        let befores = effects::befores(&judged);
        let result = match self.exchange("tool.call", params) {
            Ok(result) => result,
            Err(error) => return json!({"error": error}),
        };
        let context = Context {
            tool,
            place: &place,
            judged: &judged,
            befores: &befores,
        };
        let vetted = effects::vet(&result, &context);
        lock(&self.effects).extend(vetted.kept);
        lock(&self.dropped).extend(vetted.dropped);
        result
    }

    /// Send any core-to-extension request and wait (up to ten seconds) for the answer: the result, or `{"error": …}`.
    ///
    /// # Panics
    ///
    /// No answer in ten seconds.
    pub fn request(&self, method: &str, params: Value) -> Value {
        self.exchange(method, params)
            .unwrap_or_else(|error| json!({"error": error}))
    }

    /// Send a request and wait for its result or JSON-RPC error.
    fn exchange(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = format!("core-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let (tell, answer) = mpsc::channel();
        self.seen
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), tell);
        write(
            &self.writer,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        answer
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("no answer to {method} within ten seconds"))
    }

    /// Push a notification (`extension.config`, `tool.cancel`, `widget.watch`...).
    pub fn notify(&self, method: &str, params: Value) {
        write(
            &self.writer,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        );
    }

    /// End the extension's stdin and wait for it to return: what `run_with` returned.
    ///
    /// # Errors
    ///
    /// The extension's `run_with` failed: the handshake was refused.
    ///
    /// # Panics
    ///
    /// The extension's thread panicked.
    pub fn close(mut self) -> Result<(), String> {
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match self.extension.take() {
            Some(extension) => extension
                .join()
                .expect("the extension's thread didn't panic"),
            None => Ok(()),
        }
    }
}

impl Drop for FakeCore {
    fn drop(&mut self) {
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// Read what the extension writes: answer the handshake and its calls, hand answers to whoever waits.
fn read_all(seen: &Seen, writer: &Mutex<Option<PipeWriter>>, reader: impl BufRead) {
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        seen.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.clone());
        let id = message["id"].clone();
        match message["method"].as_str() {
            Some("hello") => write(
                writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": seen.hello}),
            ),
            Some(method) => {
                let answer = match method {
                    "provide" => provided(seen, &message["params"]),
                    "package.methods" => {
                        *seen.methods.lock().unwrap_or_else(PoisonError::into_inner) =
                            message["params"]["methods"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                        Ok(json!({}))
                    }
                    other => match seen
                        .answers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get(other)
                    {
                        Some(answer) => answer(&message["params"]),
                        None => Err(
                            json!({"code": -32601, "message": "not answered by FakeCore",
                                           "data": {"reason": "unknown_method"}}),
                        ),
                    },
                };
                let reply = match answer {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                };
                write(writer, &reply);
                // A refused `provide` ends registering too: no `package.methods` follows.
                let registered = method == "package.methods"
                    || (method == "provide"
                        && (!seen.expects_methods || lock(&seen.refused).is_some()));
                if registered
                    && let Some(done) = seen
                        .registered
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take()
                {
                    // The test stopped waiting: nothing to tell.
                    done.send(()).unwrap_or_default();
                }
            }
            None => {
                let tell = seen
                    .waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id.as_str().unwrap_or_default());
                if let Some(tell) = tell {
                    let answer = match message.get("error") {
                        Some(error) => Err(error.clone()),
                        None => Ok(message["result"].clone()),
                    };
                    drop(tell.send(answer));
                }
            }
        }
    }
}

/// `provide`: check what the tools declare, as the core does, then take them.
fn provided(seen: &Seen, params: &Value) -> Result<Value, Value> {
    let tools = params["tools"].as_array().cloned().unwrap_or_default();
    let refusal = tools
        .iter()
        .find_map(|spec| {
            effects::declared_problem(spec).map(|problem| {
                json!({"code": -32010, "message": format!("tool {}: {problem} is wrong", spec["name"]),
                       "data": {"reason": "bad_tool", "tool": spec["name"], "problem": problem}})
            })
        })
        .or_else(|| {
            let runs = tools.iter().any(|spec| spec["runs_commands"] == true);
            let approved = seen
                .approved
                .as_ref()
                .is_none_or(|approved| approved.iter().any(|one| one == "tools.commands"));
            (runs && !approved).then(|| {
                json!({"code": -32010, "message": "the tools.commands capability is not approved",
                       "data": {"reason": "needs_approval", "capabilities": ["tools.commands"]}})
            })
        });
    if let Some(refusal) = refusal {
        *lock(&seen.refused) = Some(refusal.clone());
        return Err(refusal);
    }
    let count = tools.len();
    *lock(&seen.tools) = tools;
    Ok(json!({"tools": count}))
}

/// Lock, taking over from a thread that panicked holding it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn write(writer: &Mutex<Option<PipeWriter>>, message: &Value) {
    if let Some(pipe) = writer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
        && writeln!(pipe, "{message}")
            .and_then(|()| pipe.flush())
            .is_err()
    {
        // The extension stopped reading.
    }
}
