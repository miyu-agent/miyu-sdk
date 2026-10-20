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

| Write | Does |
|---|---|
| `@ext.tool(name, description, schema, access=…)` | A tool: the function gets `(args, call)` and returns text, a `Reply` or a full answer. `call.cancelled` says whether it timed out or the person interrupted |
| `@ext.command(name)` | A slash command from the manifest's `[[slash]]`: return the text shown to the person |
| `@ext.method(name)` | A method called by your page, a widget button or another package: the function gets `(params, caller)` |
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

## License

MIT, see `LICENSE`.
