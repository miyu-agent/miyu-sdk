//! [`FakeCore`]: a core in the same process, for testing an extension without Miyu. It shakes hands (handing over the
//! config), answers `provide`, `package.methods` and whatever you add with [`FakeCore::answer`], and lets you call tools,
//! commands and methods and push notifications. Dropping it (or [`FakeCore::close`]) ends the extension's stdin.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, PipeWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::Extension;

type AnswerFn = Box<dyn Fn(&Value) -> Result<Value, Value> + Send + Sync>;

/// What the reading thread shares with the test.
struct Seen {
    received: Mutex<Vec<Value>>,
    waiting: Mutex<HashMap<String, Sender<Value>>>,
    answers: Mutex<HashMap<String, AnswerFn>>,
    tools: Mutex<Vec<Value>>,
    methods: Mutex<Vec<Value>>,
    registered: Mutex<Option<Sender<()>>>,
    expects_methods: bool,
    hello: Value,
}

/// A core in-process; see the module docs.
pub struct FakeCore {
    seen: Arc<Seen>,
    writer: Arc<Mutex<Option<PipeWriter>>>,
    extension: Option<JoinHandle<Result<(), String>>>,
    next: AtomicU64,
}

impl FakeCore {
    /// Start `ext` against a fake core that hands over `config`, and wait (up to ten seconds) until it has registered.
    ///
    /// # Panics
    ///
    /// The pipes can't be made, or the extension didn't shake hands and register in time.
    pub fn start(ext: Extension, config: Map<String, Value>) -> FakeCore {
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
        });
        let writer = Arc::new(Mutex::new(Some(to_ext)));
        let extension = std::thread::spawn(move || {
            ext.run_with(BufReader::new(into_ext), Box::new(out_of_ext))
        });
        let (reading, written) = (Arc::clone(&seen), Arc::clone(&writer));
        std::thread::spawn(move || read_all(&reading, &written, BufReader::new(from_ext)));
        done.recv_timeout(Duration::from_secs(10))
            .expect("the extension shook hands and registered within ten seconds");
        FakeCore {
            seen,
            writer,
            extension: Some(extension),
            next: AtomicU64::new(0),
        }
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

    /// Call `tool` the way the core does; the result, or `{"error": …}`.
    pub fn call_tool(&self, tool: &str, args: Value) -> Value {
        let call_id = format!("c{}", self.next.load(Ordering::Relaxed) + 1);
        self.call_tool_as(tool, args, &call_id)
    }

    /// Same as [`FakeCore::call_tool`] with a chosen `call_id` (to cancel it).
    pub fn call_tool_as(&self, tool: &str, args: Value, call_id: &str) -> Value {
        let params = json!({"session": "s1", "call_id": call_id, "tool": tool, "args": args, "cwd": "/", "owner": true});
        self.request("tool.call", params)
    }

    /// Send any core-to-extension request and wait (up to ten seconds) for the answer: the result, or `{"error": …}`.
    ///
    /// # Panics
    ///
    /// No answer in ten seconds.
    pub fn request(&self, method: &str, params: Value) -> Value {
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
                    "provide" => {
                        *seen.tools.lock().unwrap_or_else(PoisonError::into_inner) =
                            message["params"]["tools"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                        Ok(
                            json!({"tools": seen.tools.lock().unwrap_or_else(PoisonError::into_inner).len()}),
                        )
                    }
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
                let registered =
                    method == "package.methods" || (method == "provide" && !seen.expects_methods);
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
                        Some(error) => json!({"error": error}),
                        None => message["result"].clone(),
                    };
                    drop(tell.send(answer));
                }
            }
        }
    }
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
