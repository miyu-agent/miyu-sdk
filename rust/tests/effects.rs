//! 规格多的三格、`checked`、`channel`、回应的 `effects`（核心设计 63 第二到六节），经假核心走协议，和 Python 那份的
//! `Declarations`、`Effects`、`Commands` 一一对应；例子 `examples/replace.rs` 也照它跑一遍。

// `allow-unwrap-in-tests` covers `#[test]` functions only; the helpers here are test code too.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use miyu_ext::{Change, Extension, FakeCore, Reply, Tool, content_hash};

#[path = "../examples/replace.rs"]
#[allow(dead_code)]
mod replace;

/// 一个测试用的临时目录（真实位置），丢掉时删掉。
struct Folder(PathBuf);

impl Folder {
    fn new() -> Folder {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "miyu-ext-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).unwrap();
        Folder(real(&path))
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn text(&self, name: &str) -> String {
        self.join(name).to_string_lossy().into_owned()
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.join(name), text).unwrap();
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

/// 真实位置，去掉 Windows 的 `\\?\`（同假核心交出去的 `checked`）。
fn real(path: &Path) -> PathBuf {
    let real = std::fs::canonicalize(path).unwrap();
    let text = real.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => real,
    }
}

fn edit_schema() -> Value {
    json!({"type": "object", "properties": {
        "file_path": {"type": "string"}, "files": {"type": "array", "items": {"type": "string"}},
        "run_in_background": {"type": "boolean"}, "command": {"type": "string"}}, "required": ["file_path"]})
}

fn text(answer: &Value) -> &str {
    answer["blocks"][0]["text"].as_str().unwrap_or_default()
}

#[test]
fn the_three_settings_go_into_the_spec() {
    let mut ext = Extension::new("d", "0.0.0");
    ext.tool(
        Tool::new("my_edit", "Edit.")
            .schema(edit_schema())
            .access("write")
            .paths(json!([{"parameter": "file_path", "write": true}]))
            .background_parameters(&["run_in_background"])
            .runs_commands(true),
        |_, _| Reply::text("ok"),
    )
    .tool(Tool::new("plain", "Plain."), |_, _| Reply::text("ok"));
    let core = FakeCore::start(ext, Map::new());
    let tools = core.tools();
    assert_eq!(
        tools[0]["paths"],
        json!([{"parameter": "file_path", "write": true}])
    );
    assert_eq!(
        tools[0]["background_parameters"],
        json!(["run_in_background"])
    );
    assert_eq!(tools[0]["runs_commands"], true);
    let keys: Vec<&String> = tools[1].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["access", "description", "input_schema", "name", "venues"],
        "a tool without them is as before"
    );
    assert_eq!(core.refused(), None);
    assert_eq!(core.close(), Ok(()));
}

/// 登记一件带 `tool` 这些设置的工具，交回 `provide` 被拒的错误。
fn refused(tool: Tool, approved: Option<&[&str]>) -> Option<Value> {
    let mut ext = Extension::new("d", "0.0.0");
    ext.tool(tool, |_, _| Reply::text("ok"));
    let core = match approved {
        Some(approved) => FakeCore::start_approved(ext, Map::new(), approved),
        None => FakeCore::start(ext, Map::new()),
    };
    core.refused()
}

fn my_edit() -> Tool {
    Tool::new("my_edit", "Edit.")
        .schema(edit_schema())
        .access("write")
}

#[test]
fn wrong_declarations_are_bad_tool() {
    for (problem, tool) in [
        ("paths", my_edit().paths(json!([{"parameter": "nowhere"}]))),
        (
            "paths",
            my_edit().paths(json!([{"parameter": "run_in_background"}])),
        ),
        (
            "paths",
            my_edit()
                .paths(json!([{"parameter": "file_path", "write": true}]))
                .access("read"),
        ),
        (
            "paths",
            my_edit().paths(json!([{"parameter": "file_path", "mode": "w"}])),
        ),
        (
            "paths",
            my_edit().paths(json!([{"parameter": "file_path"}, {"parameter": "file_path"}])),
        ),
        ("paths", my_edit().paths(json!({"parameter": "file_path"}))),
        (
            "background_parameters",
            my_edit().background_parameters(&["file_path"]),
        ),
        (
            "background_parameters",
            my_edit().background_parameters(&["nowhere"]),
        ),
    ] {
        let refused = refused(tool, None).expect("refused");
        assert_eq!(
            refused["data"],
            json!({"reason": "bad_tool", "tool": "my_edit", "problem": problem})
        );
    }
    // `runs_commands` 不是布尔的只能手写规格造：设置本身只收布尔。
    let raw = refused_raw(
        json!({"name": "my_edit", "description": "Edit.", "input_schema": edit_schema(),
        "access": "write", "venues": ["local"], "runs_commands": "yes"}),
    );
    assert_eq!(
        raw["data"],
        json!({"reason": "bad_tool", "tool": "my_edit", "problem": "runs_commands"})
    );
    assert_eq!(
        refused(
            my_edit()
                .paths(json!([{"parameter": "files"}]))
                .access("read"),
            None
        ),
        None,
        "an array of strings is fine"
    );
}

/// 用一份手写的规格登记：经扩展的 `ready` 再 `provide` 一遍（同一个连接可以重新登记），交回假核心记下的拒绝。
fn refused_raw(spec: Value) -> Value {
    let mut ext = Extension::new("d", "0.0.0");
    ext.ready(move |core| {
        drop(core.call("provide", json!({"tools": [spec.clone()]})));
    });
    let core = FakeCore::start(ext, Map::new());
    for _ in 0..1000 {
        if let Some(refused) = core.refused() {
            return refused;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("no refusal");
}

#[test]
fn running_commands_needs_tools_commands_approved() {
    let refusal = refused(my_edit().runs_commands(true), Some(&[])).expect("refused");
    assert_eq!(
        refusal["data"],
        json!({"reason": "needs_approval", "capabilities": ["tools.commands"]})
    );
    assert_eq!(
        refused(my_edit().runs_commands(true), Some(&["tools.commands"])),
        None
    );
    assert_eq!(refused(my_edit().runs_commands(false), Some(&[])), None);
}

/// 下一次调用扩展要做的：写哪些文件、删哪些、算不算出错、报什么效果。
#[derive(Default, Clone)]
struct Next {
    write: Vec<(&'static str, &'static str)>,
    remove: Vec<&'static str>,
    error: bool,
    effects: Vec<Value>,
}

/// 扩展收到的一次调用：`checked`、`channel`。
type Seen = (Vec<String>, Option<miyu_ext::Channel>);

/// 三件工具（声明写路径和读路径的、只读的、没声明的）照 [`Next`] 做，记下收到的调用参数。
struct Bench {
    folder: Folder,
    core: FakeCore,
    next: Arc<Mutex<Next>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Bench {
    fn new() -> Bench {
        let folder = Folder::new();
        std::fs::create_dir_all(folder.join("home")).unwrap();
        let next = Arc::new(Mutex::new(Next::default()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut ext = Extension::new("e", "0.0.0");
        for tool in [
            my_edit()
                .paths(json!([{"parameter": "file_path", "write": true}, {"parameter": "files"}])),
            Tool::new("my_read", "Read.")
                .schema(edit_schema())
                .paths(json!([{"parameter": "file_path"}])),
            my_edit_named("no_paths"),
        ] {
            let (next, seen, root) = (Arc::clone(&next), Arc::clone(&seen), folder.0.clone());
            ext.tool(tool, move |_, call| {
                seen.lock().unwrap().push((
                    call.checked().iter().map(|one| one.to_string()).collect(),
                    call.channel(),
                ));
                let next = std::mem::take(&mut *next.lock().unwrap());
                for (name, text) in next.write {
                    std::fs::write(root.join(name), text).unwrap();
                }
                for name in next.remove {
                    std::fs::remove_file(root.join(name)).unwrap();
                }
                let reply = if next.error {
                    Reply::failed("done")
                } else {
                    Reply::text("done")
                };
                reply.effects(next.effects)
            });
        }
        let core = FakeCore::start(ext, Map::new());
        core.set_cwd(&folder.0);
        core.set_home(&folder.join("home"));
        Bench {
            folder,
            core,
            next,
            seen,
        }
    }

    fn call(&self, tool: &str, args: Value, next: Next) -> Value {
        *self.next.lock().unwrap() = next;
        self.core.clear_effects();
        self.core.call_tool(tool, args)
    }

    fn edit(&self, next: Next) -> Value {
        self.call("my_edit", json!({"file_path": "a.txt"}), next)
    }

    fn whys(&self) -> Vec<(String, String)> {
        self.core
            .dropped()
            .iter()
            .map(|one| {
                (
                    one["kind"].as_str().unwrap().to_string(),
                    one["why"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn last_checked(&self) -> Vec<String> {
        self.seen.lock().unwrap().last().unwrap().0.clone()
    }
}

fn my_edit_named(name: &str) -> Tool {
    Tool::new(name, "Edit.")
        .schema(edit_schema())
        .access("write")
}

fn whys(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(kind, why)| (kind.to_string(), why.to_string()))
        .collect()
}

fn effects_of(reply: Reply) -> Vec<Value> {
    // 回应的 JSON 只能经假核心看到：这里照 `Reply` 的写法直接拼出 `effects`。
    let mut ext = Extension::new("x", "0.0.0");
    ext.tool(Tool::new("x", "X."), move |_, _| reply.clone());
    let core = FakeCore::start(ext, Map::new());
    core.call_tool("x", json!({}))["effects"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn reply_helpers_write_the_protocol() {
    let effects = effects_of(
        Reply::text("x")
            .changed("a.txt")
            .changed_with(
                "b.txt",
                Change {
                    before: Some(content_hash(b"old")),
                    after_text: Some("new".to_string()),
                    ..Change::default()
                },
            )
            .read("c.txt", Some([1, 2]))
            .read("d.txt", None)
            .trashed("e.txt", "/trash/e.txt")
            .todos(
                json!([{"content": "x", "status": "pending"}]),
                Some(json!([])),
            ),
    );
    assert_eq!(
        effects,
        [
            json!({"kind": "file.changed", "path": "a.txt"}),
            json!({"kind": "file.changed", "path": "b.txt", "before": content_hash(b"old"), "after_text": "new"}),
            json!({"kind": "file.read", "path": "c.txt", "lines": [1, 2]}),
            json!({"kind": "file.read", "path": "d.txt"}),
            json!({"kind": "file.trashed", "path": "e.txt", "trash": "/trash/e.txt"}),
            json!({"kind": "todo.written", "todos": [{"content": "x", "status": "pending"}], "done": []}),
        ]
    );
    assert_eq!(
        content_hash(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn checked_follows_the_declared_paths() {
    let bench = Bench::new();
    std::fs::create_dir_all(bench.folder.join("sub")).unwrap();
    bench.call(
        "my_edit",
        json!({"file_path": "~/n.txt", "files": ["sub/../b.txt", bench.folder.text("c.txt"), 3]}),
        Next::default(),
    );
    assert_eq!(
        bench.last_checked(),
        [
            bench.folder.join("home").join("n.txt").to_string_lossy(),
            bench.folder.join("b.txt").to_string_lossy(),
            bench.folder.join("c.txt").to_string_lossy(),
        ],
        "in order; ~ is home; non-strings left out"
    );
    bench.call("no_paths", json!({"file_path": "a.txt"}), Next::default());
    assert!(
        bench.last_checked().is_empty(),
        "no paths declared: no checked"
    );
    assert_eq!(
        bench.seen.lock().unwrap().last().unwrap().1,
        None,
        "doesn't run commands: no channel"
    );
}

#[cfg(unix)]
#[test]
fn checked_follows_links() {
    let bench = Bench::new();
    std::fs::create_dir_all(bench.folder.join("there")).unwrap();
    std::os::unix::fs::symlink(bench.folder.join("there"), bench.folder.join("link")).unwrap();
    bench.call(
        "my_edit",
        json!({"file_path": "link/a.txt"}),
        Next::default(),
    );
    assert_eq!(
        bench.last_checked(),
        [bench.folder.join("there").join("a.txt").to_string_lossy()]
    );
}

#[test]
fn a_change_inside_checked_is_kept() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "old");
    bench.edit(Next {
        write: vec![("a.txt", "new")],
        effects: vec![json!({"kind": "file.changed", "path": "a.txt",
            "before": content_hash(b"old"), "after_text": "new"})],
        ..Next::default()
    });
    assert_eq!(
        bench.core.effects(),
        [json!({"kind": "file.changed", "path": bench.folder.text("a.txt")})]
    );
    assert!(bench.core.dropped().is_empty());
    bench.call(
        "my_edit",
        json!({"file_path": "n.txt"}),
        Next {
            write: vec![("n.txt", "made")],
            effects: vec![json!({"kind": "file.changed", "path": "n.txt"})],
            ..Next::default()
        },
    );
    assert_eq!(
        bench.core.effects(),
        [json!({"kind": "file.changed", "path": bench.folder.text("n.txt")})],
        "a new file"
    );
}

#[test]
fn paths_outside_checked_are_not_checked() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "a");
    bench.folder.write("other.txt", "o");
    bench.edit(Next {
        write: vec![("other.txt", "changed")],
        effects: vec![
            json!({"kind": "file.changed", "path": "other.txt"}),
            json!({"kind": "file.read", "path": "other.txt"}),
        ],
        ..Next::default()
    });
    assert_eq!(
        bench.whys(),
        whys(&[
            ("file.changed", "not_checked"),
            ("file.read", "not_checked")
        ])
    );
    assert!(bench.core.effects().is_empty());
    bench.call(
        "no_paths",
        json!({"file_path": "a.txt"}),
        Next {
            write: vec![("a.txt", "b")],
            effects: vec![json!({"kind": "file.changed", "path": "a.txt"})],
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[("file.changed", "not_checked")]),
        "no paths declared: no file effects"
    );
}

#[test]
fn changing_and_trashing_need_a_write_path() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "a");
    bench.folder.write("r.txt", "r");
    bench.call(
        "my_edit",
        json!({"file_path": "a.txt", "files": ["r.txt"]}),
        Next {
            write: vec![("r.txt", "changed")],
            remove: vec!["a.txt"],
            effects: vec![
                json!({"kind": "file.changed", "path": "r.txt"}),
                json!({"kind": "file.read", "path": "r.txt", "lines": [1, 1]}),
                json!({"kind": "file.trashed", "path": "a.txt", "trash": "/trash/a.txt"}),
            ],
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[("file.changed", "not_checked")]),
        "files is declared for reading only"
    );
    assert_eq!(
        bench.core.effects(),
        [
            json!({"kind": "file.read", "path": bench.folder.text("r.txt"), "lines": [1, 1]}),
            json!({"kind": "file.trashed", "path": bench.folder.text("a.txt"), "trash": "/trash/a.txt"}),
        ]
    );
    bench.folder.write("a.txt", "a");
    bench.call(
        "my_read",
        json!({"file_path": "a.txt"}),
        Next {
            write: vec![("a.txt", "b")],
            effects: vec![
                json!({"kind": "file.changed", "path": "a.txt"}),
                json!({"kind": "file.trashed", "path": "a.txt", "trash": "t"}),
            ],
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[
            ("file.changed", "not_checked"),
            ("file.trashed", "not_checked")
        ])
    );
}

#[test]
fn only_four_kinds() {
    let bench = Bench::new();
    bench.edit(Next {
        effects: vec![
            json!({"kind": "job.started", "job": "x"}),
            json!({"kind": "file.shown", "path": "a.txt"}),
            json!({"kind": "package.state"}),
        ],
        ..Next::default()
    });
    assert_eq!(
        bench.whys(),
        whys(&[
            ("job.started", "kind"),
            ("file.shown", "kind"),
            ("package.state", "kind")
        ])
    );
    bench.edit(Next {
        effects: vec![
            json!({"kind": "todo.written", "todos": [{"content": "Write tests", "status": "in_progress"}]}),
        ],
        ..Next::default()
    });
    assert_eq!(
        bench.core.effects(),
        [
            json!({"kind": "todo.written", "todos": [{"content": "Write tests", "status": "in_progress"}]}),
        ]
    );
}

#[test]
fn wrong_shapes_are_dropped() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "a");
    bench.edit(Next {
        write: vec![("a.txt", "b")],
        effects: vec![
            json!("file.changed"),
            json!({"path": "a.txt"}),
            json!({"kind": "file.changed"}),
            json!({"kind": "file.changed", "path": 1}),
            json!({"kind": "file.changed", "path": "a.txt", "before": "md5:x"}),
            json!({"kind": "file.read", "path": "a.txt", "lines": [0, 3]}),
            json!({"kind": "file.read", "path": "a.txt", "lines": [3, 2]}),
            json!({"kind": "file.trashed", "path": "a.txt", "trash": "x".repeat(4097)}),
            json!({"kind": "todo.written", "todos": [{"content": "x"}]}),
            json!({"kind": "todo.written", "todos": [], "done": null}),
            json!({"kind": "file.changed", "path": "a.txt"}),
            json!({"kind": "file.changed", "path": "a.txt"}),
        ],
        ..Next::default()
    });
    assert_eq!(
        bench.whys(),
        whys(&[
            ("-", "shape"),
            ("-", "shape"),
            ("file.changed", "shape"),
            ("file.changed", "shape"),
            ("file.changed", "shape"),
            ("file.read", "shape"),
            ("file.read", "shape"),
            ("file.trashed", "shape"),
            ("todo.written", "shape"),
            ("todo.written", "shape"),
            ("file.changed", "shape"),
        ]),
        "the second report of the same file too"
    );
    assert_eq!(bench.core.effects().len(), 1);
}

#[test]
fn the_core_reads_the_file_itself() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "same");
    let changed = |change: Value| vec![change];
    bench.edit(Next {
        write: vec![("a.txt", "same")],
        effects: changed(json!({"kind": "file.changed", "path": "a.txt"})),
        ..Next::default()
    });
    assert_eq!(bench.whys(), whys(&[("file.changed", "unchanged")]));
    bench.edit(Next {
        write: vec![("a.txt", "new")],
        effects: changed(json!({"kind": "file.changed", "path": "a.txt", "before_text": "not it"})),
        ..Next::default()
    });
    assert_eq!(bench.whys(), whys(&[("file.changed", "mismatch")]));
    bench.edit(Next {
        write: vec![("a.txt", "newer")],
        effects: changed(
            json!({"kind": "file.changed", "path": "a.txt", "after": content_hash(b"x")}),
        ),
        ..Next::default()
    });
    assert_eq!(bench.whys(), whys(&[("file.changed", "mismatch")]));
    bench.call(
        "my_edit",
        json!({"file_path": "n.txt"}),
        Next {
            write: vec![("n.txt", "made")],
            effects: changed(json!({"kind": "file.changed", "path": "n.txt", "before_text": ""})),
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[("file.changed", "mismatch")]),
        "a new file had no before"
    );
    bench.edit(Next {
        effects: changed(json!({"kind": "file.trashed", "path": "a.txt", "trash": "t"})),
        ..Next::default()
    });
    assert_eq!(
        bench.whys(),
        whys(&[("file.trashed", "mismatch")]),
        "still there"
    );
    bench.call(
        "my_edit",
        json!({"file_path": "gone.txt"}),
        Next {
            effects: changed(json!({"kind": "file.changed", "path": "gone.txt"})),
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[("file.changed", "mismatch")]),
        "not there after"
    );
}

#[test]
fn too_big() {
    let bench = Bench::new();
    let file = std::fs::File::create(bench.folder.join("big")).unwrap();
    file.set_len(20 * 1024 * 1024 + 1).unwrap();
    drop(file);
    bench.call(
        "my_edit",
        json!({"file_path": "big"}),
        Next {
            effects: vec![
                json!({"kind": "file.changed", "path": "big"}),
                json!({"kind": "file.read", "path": "big"}),
            ],
            ..Next::default()
        },
    );
    assert_eq!(
        bench.whys(),
        whys(&[("file.changed", "too_big"), ("file.read", "too_big")])
    );
}

#[test]
fn an_error_keeps_files_but_not_todos() {
    let bench = Bench::new();
    bench.folder.write("a.txt", "a");
    bench.edit(Next {
        error: true,
        write: vec![("a.txt", "half")],
        effects: vec![
            json!({"kind": "file.changed", "path": "a.txt"}),
            json!({"kind": "todo.written", "todos": [{"content": "x", "status": "pending"}]}),
        ],
        ..Next::default()
    });
    assert_eq!(
        bench.core.effects(),
        [json!({"kind": "file.changed", "path": bench.folder.text("a.txt")})]
    );
    assert!(
        bench.core.dropped().is_empty(),
        "the to-do list is left out without a word"
    );
}

#[test]
fn the_channel_reaches_the_command() {
    let mut ext = Extension::new("c", "0.0.0");
    ext.tool(
        Tool::new("my_shell", "Run.")
            .schema(edit_schema())
            .access("execute")
            .runs_commands(true),
        |_, call| {
            let env: Map<String, Value> = call
                .command_env()
                .into_iter()
                .map(|(key, value)| (key, json!(value.to_string_lossy())))
                .collect();
            Reply::text(Value::Object(env).to_string())
        },
    );
    let core = FakeCore::start_approved(ext, Map::new(), &["tools.commands"]);
    let channel = core.channel().unwrap();
    let env: Value = serde_json::from_str(text(&core.call_tool("my_shell", json!({})))).unwrap();
    assert_eq!(env["MIYU_CALL_SOCK"], channel["sock"]);
    assert_eq!(env["MIYU_CALL_TOKEN"], channel["token"]);
    let path = env["PATH"].as_str().unwrap();
    let first = std::env::split_paths(path).next().unwrap();
    assert_eq!(
        first,
        PathBuf::from(channel["bin"].as_str().unwrap()),
        "bin first on PATH"
    );
    let rest: Vec<PathBuf> = std::env::split_paths(path).skip(1).collect();
    let before: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    assert_eq!(rest, before, "the rest of PATH kept");
    core.set_channel(Some(json!({"sock": "/s", "token": "t"})));
    let env: Value = serde_json::from_str(text(&core.call_tool("my_shell", json!({})))).unwrap();
    assert_eq!(
        env,
        json!({"MIYU_CALL_SOCK": "/s", "MIYU_CALL_TOKEN": "t"}),
        "no bin: PATH untouched"
    );
    core.set_channel(None);
    assert_eq!(
        text(&core.call_tool("my_shell", json!({}))),
        "{}",
        "no channel: nothing set"
    );
}

#[test]
fn replace_edits_the_checked_file_and_reports_it() {
    let folder = Folder::new();
    folder.write("a.txt", "hello world\n");
    let core = FakeCore::start(replace::extension(), Map::new());
    core.set_cwd(&folder.0);
    assert_eq!(
        core.tools()[0]["paths"],
        json!([{"parameter": "file_path", "write": true}])
    );
    let answer = core.call_tool(
        "replace_text",
        json!({"file_path": "a.txt", "old": "world", "new": "there"}),
    );
    assert_eq!(text(&answer), "Replaced in a.txt.");
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "hello there\n"
    );
    assert_eq!(
        core.effects(),
        [json!({"kind": "file.changed", "path": folder.text("a.txt")})],
        "kept, like the built-in edit"
    );
    assert!(core.dropped().is_empty());
    let same = core.call_tool(
        "replace_text",
        json!({"file_path": "a.txt", "old": "there", "new": "there"}),
    );
    assert_eq!(same.get("error"), None);
    assert_eq!(
        core.dropped(),
        [json!({"tool": "replace_text", "kind": "file.changed", "why": "unchanged"})]
    );
    let missing = core.call_tool(
        "replace_text",
        json!({"file_path": "a.txt", "old": "nowhere", "new": "x"}),
    );
    assert_eq!(missing["error"], true);
    assert_eq!(core.close(), Ok(()));
}
