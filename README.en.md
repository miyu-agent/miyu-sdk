# Miyu SDK

Two small libraries for writing Miyu extensions: Python (`python/miyu_ext.py`) and Rust (`rust/`, crate `miyu-ext`), with the same concepts and names. The Chinese [README.md](README.md) is the original.

What an extension can do is set by the protocol (`docs/reference/extensions.en.md` in the core repository); the SDK adds nothing on top. It wraps what every extension would otherwise write again: the handshake, line-by-line messages, answering concurrent calls by id, cancelling, config pushes, exiting when stdin ends, and the exit code 1 rule.

## Python

One file, standard library only, Python 3.9 or newer. Copy it into your package's `bin/`, next to the program:

```text
hello/
├── package.toml
└── bin/
    ├── miyu-hello        # your program, first line #!/usr/bin/env python3
    └── miyu_ext.py       # the copied SDK
```

```python
#!/usr/bin/env python3
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from miyu_ext import Extension

ext = Extension("hello", "0.1.0")

@ext.tool("hello_greet", "Greet someone by name.",
          {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]},
          access="read", display_name={"en": "Greet", "zh": "打招呼"}, subject="name")
def greet(args, call):
    return f"{ext.config.get('hello.greeting', 'Hello')}, {args['name']}!"

if __name__ == "__main__":
    ext.run()
```

The full example is in `python/examples/hello/`; the manifest is described in `docs/authors/manifest.en.md` in the core repository.

One file for all three platforms: Linux and macOS start it by its `#!` line (the program needs execute permission); on Windows the core reads the `#!` line to find the interpreter, and `python3` means `python` (core 0.8.0 on). Miyu doesn't ship Python: say in your package's description that it needs Python 3.9 or later.

| Write | Does |
|---|---|
| `@ext.tool(name, description, schema, access=…)` | A tool: the function gets `(args, call)` and returns text, a `Reply` or a full answer. `call.cancelled` says whether it timed out or the person interrupted |
| `Reply(text, error, data, detail, sent, end_turn)` | A full answer: `data` goes only to scripts that call the tool with `miyu call` (one line of JSON), `detail` only to heads for cards, `sent` are images she sends, `end_turn` ends the turn here |
| `@ext.tool(…, paths=…, background_parameters=…, runs_commands=…)` | Three more spec settings: which parameters are paths (`[{"parameter": "file_path", "write": True}]`), parameters only given when the session can run things in the background, and that the tool runs commands. See "Tools that edit files or run commands" below |
| `Reply(…).changed(path)`, `.read(path, lines)`, `.trashed(path, trash)`, `.todos(todos, done)` | The answer's `effects`: files changed, read or moved to the trash, and the to-do list it wrote. `changed` can carry `before`, `after` (`content_hash(…)`) or `before_text`, `after_text` as an extra check |
| `call.checked`, `call.channel`, `call.command_env()` | The real paths the core's check allowed (tools declaring `paths`); the `miyu call` channel (tools declaring `runs_commands`) and an environment for child processes with it set |
| `@ext.command(name)` | A slash command from the manifest's `[[slash]]`: return the text shown to the person |
| `@ext.method(name, timeout_ms=None, later=False)` | A method called by your page, a widget button, a head or another package (by bus name or package id): the function gets `(params, caller)`; raise `MethodError(code, message)` for an error with a code (it goes in the error's `data.code`); `timeout_ms` is how long the core waits (1000 to 600000, 30 seconds by default); `later=True` leaves it unregistered until `ext.enable_methods([...])` (say, once a model has downloaded), and `ext.disable_methods([...])` takes it back |
| `@ext.on(method)` | Other calls and pushes from the core: `user.append`, `search.query`, `widget.watch`… |
| `@ext.on_config`, `ext.config` | Config pushes; the handshake's config is in `ext.config` |
| `@ext.ready` | Runs once on its own thread after the handshake and registration |
| `ext.call(method, params)` | Calls a core method (`blob.put`, `model.call`…); a refusal raises `CoreError` with the code in `reason` |
| `ext.fatal(message)` | For problems a restart won't fix (bad settings, port taken): writes stderr and exits with 1, so the core stops retrying |

The handshake checks the protocol major version and, on a mismatch, explains it and exits under the exit code 1 rule. Declare capabilities honestly in the manifest: calling a method whose capability isn't declared returns `needs_approval`.

**Testing**: no Miyu needed; `FakeCore` plays the core in-process:

```python
from miyu_ext import FakeCore
core = FakeCore(ext, config={"hello.greeting": "Hi"})
assert core.call_tool("hello_greet", {"name": "Ann"})["blocks"][0]["text"] == "Hi, Ann!"
core.close()
```

The fake core checks the three spec settings the way the core does (a wrong one is `bad_tool`; a tool that runs commands without `tools.commands` in `approved` is `needs_approval`, and by default everything is approved); a refusal is in `core.refused`. `call_tool` sends `checked` (the declared paths resolved against `core.cwd` and `core.home`) and `channel` (a fake one, `core.channel`, not served) as the core does, and vets the answer's `effects` as the core does: the kept ones go to `core.effects` (paths as real locations), the dropped ones to `core.dropped`, one `{"tool", "kind", "why"}` each.

This repository's own tests: `python3 -m unittest discover -s python/tests`.

## Rust

Crate `miyu-ext`: synchronous, depends only on `serde_json`. Handlers call core methods and read config through `call.core()` (commands, methods and other handlers get `&Core` directly), like Python's `ext.call` and `ext.config`.

```rust
use miyu_ext::{Extension, Reply, Tool};
use serde_json::{Value, json};

let mut ext = Extension::new("hello", "0.1.0");
ext.tool(
    Tool::new("hello_greet", "Greet someone by name.")
        .schema(json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}))
        .subject("name"),
    |args, call| {
        let config = call.core().config();
        let greeting = config.get("hello.greeting").and_then(Value::as_str).unwrap_or("Hello");
        Reply::text(format!("{greeting}, {}!", args["name"].as_str().unwrap_or("there")))
    },
);
ext.run();
```

The full example is `rust/examples/hello.rs` (`cargo build --example hello`; put the binary in the package's `bin/` as `miyu-hello`, `miyu-hello.exe` on Windows). `FakeCore::start(ext, config)` works like the Python fake core; tests: `cargo test` in `rust/`.

The names in both kits (for the next section):

| Python | Rust |
|---|---|
| `@ext.tool(…, paths=[…], background_parameters=[…], runs_commands=True)` | `Tool::paths(json!([…]))`, `.background_parameters(&[…])`, `.runs_commands(true)` |
| `Reply(effects=[…])` | `Reply::effects(vec![…])` |
| `.changed(path)`, `.changed(path, before=…, after=…, before_text=…, after_text=…)` | `.changed(path)`, `.changed_with(path, Change { before, after, before_text, after_text })` |
| `.read(path, lines=(1, 20))`, `.trashed(path, trash)`, `.todos(todos, done=None)` | `.read(path, Some([1, 20]))`, `.trashed(path, trash)`, `.todos(todos, None)` |
| `content_hash(data)` | `content_hash(&data)` |
| `call.checked` (a list), `call.channel` (a dict or `None`) | `call.checked()` (`Vec<&str>`), `call.channel()` (`Option<Channel>`) |
| `call.command_env()`: the whole environment (a copy of `os.environ` plus the channel) | `call.command_env()`: only what to add, for `Command::envs` |
| `FakeCore(ext, approved=[…], cwd=…, home=…)`, `core.channel = …` | `FakeCore::start_approved(ext, config, &[…])`, `set_cwd`, `set_home`, `set_channel` |
| `core.refused`, `core.effects`, `core.dropped` | `core.refused()`, `core.effects()`, `core.dropped()`, `clear_effects()` |
| `@ext.method(name, timeout_ms=…, later=True)`, `raise MethodError(code, message)` | `ext.method_with(Method::new(name).timeout_ms(…).later(), …)`, `Err(MethodError::new(code, message))` (a bare name and `Err(String)` work as before) |
| `ext.enable_methods([…])`, `ext.disable_methods([…])` | `core.enable_methods(&[…])`, `core.disable_methods(&[…])` |

## Tools that edit files or run commands

From core XG-1 (2490fb77, 514bdc40) on. The protocol is in the core repository's `docs/reference/extensions.en.md` (`provide`, and "Effects" and "Running commands" under `tool.call`); how to write them is in "Tools that change files" and "Tools that run commands" in `docs/authors/extensions.en.md`.

**Editing files** (editors, formatters, renamers): to get diffs and line counts like the built-in `edit`, and so a file she read and you changed needn't be read again:

1. Declare `paths` in the spec, with `write` access: the core checks them (writing outside the workspace asks the person first; you get no call before they agree).
2. Write only the locations in `call.checked`: the real locations the check allowed, in the order of the paths in the arguments. Don't write the argument's path yourself (a link on it may have been swapped since the check).
3. Report `changed(path)` in the answer: only which file; the core reads the content itself (before, as it sends the call; after, when you answer). An unchanged file, a path not in `checked`, a change or trash on a path checked for reading only, an unknown kind or a wrong shape: the core drops that one effect and the result stands.

```python
@ext.tool("replace_text", "Replace one exact piece of text in a file.", SCHEMA, access="write",
          paths=[{"parameter": "file_path", "write": True}])
def replace(args, call):
    path = call.checked[0]
    ...  # edit path
    return Reply(text="Replaced.").changed(path)
```

Full examples: `python/examples/replace/` and `rust/examples/replace.rs`; the tests run them against the fake core.

**Running commands** (replacing the built-in `shell`, running in a container): add the `tools.commands` capability to the manifest and `runs_commands=True` to the spec. When the session has tools scripts can call, `call.channel` is this call's channel; start commands with `call.command_env()` as their environment (it sets `MIYU_CALL_SOCK`, `MIYU_CALL_TOKEN` and puts `bin` first on `PATH`), and `miyu call` in them calls tools as this session. The command must be started by your extension process (a descendant), and you answer after it ends: the channel closes with the answer.

```python
subprocess.run(command, shell=True, cwd=call.cwd, env=call.command_env())
```

```rust
std::process::Command::new("sh").args(["-c", command]).envs(call.command_env()).status()
```

## Versions

The current version is v0.1.0. Each release gets a `vX.Y.Z` tag, with `__version__` in `python/miyu_ext.py` and the version in `rust/Cargo.toml` updated to match; a tag is made only after both kits pass their tests on all three platforms (`.github/workflows/ci.yml`). For Rust, depend on the tag:

```toml
[dependencies]
miyu-ext = { git = "https://github.com/miyu-agent/miyu-sdk", tag = "v0.1.0" }
```

## License

Apache-2.0, see `LICENSE` (since 2026-10-26; MIT before). Extensions you write with it can use any license: they only talk to Miyu over its protocol, so they are separate works and the core's AGPL does not reach them.
