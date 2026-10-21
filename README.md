# Miyu 开发包

写 Miyu 扩展用的两个小库：Python（`python/miyu_ext.py`）、Rust（`rust/`，crate `miyu-ext`）。两份的概念、名字一一对应。英文版：[README.en.md](README.en.md)。

扩展能做什么由协议定（核心仓库的 `docs/reference/extensions.md`），开发包不多给本事；它把每个扩展都要写一遍的那部分包起来：握手、一行一条的收发、几个调用同时来时照编号对着回、取消、配置推送、标准输入读到头就退、退出码 1 的规矩。

## Python

一个文件、只用标准库，Python 3.9 起。拷进你的包的 `bin/`，和程序放在一起：

```text
hello/
├── package.toml
└── bin/
    ├── miyu-hello        # 你的程序，第一行 #!/usr/bin/env python3
    └── miyu_ext.py       # 拷过来的开发包
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

完整的例子在 `python/examples/hello/`，清单怎么写见核心仓库的 `docs/authors/manifest.md`。

三个平台一份文件：Linux、macOS 上照 `#!` 直接起（程序要有可执行权限）；Windows 上核心照 `#!` 找解释器，`python3` 找的是 `python`（核心 0.8.0 起）。Miyu 不带 Python：请在包的说明里写上要装 Python 3.9 以上。

| 写什么 | 做什么 |
|---|---|
| `@ext.tool(name, description, schema, access=…)` | 一件工具：函数收 `(args, call)`，交回字、`Reply` 或者完整的回答。`call.cancelled` 是超时、人打断了没有 |
| `Reply(text, error, data, detail, sent, end_turn)` | 完整的回答：`data` 只给 AI 在命令里用 `miyu call` 调到它的脚本（一行 JSON），`detail` 只给头画卡片，`sent` 是她发出去的图，`end_turn` 这一轮到此为止 |
| `@ext.command(name)` | 清单 `[[slash]]` 里的一条斜杠命令：交回给人看的那段话 |
| `@ext.method(name)` | 后台页、界面块的按钮、别的包调的方法：函数收 `(params, caller)` |
| `@ext.on(method)` | 别的反向调用、推送：`user.append`、`search.query`、`widget.watch`…… |
| `@ext.on_config`、`ext.config` | 配置推送；握手时的配置在 `ext.config` |
| `@ext.ready` | 握手、登记完以后在自己的线程里跑一次 |
| `ext.call(method, params)` | 调核心的方法（`blob.put`、`model.call`……）；被拒的抛 `CoreError`，`reason` 是原因码 |
| `ext.fatal(message)` | 配置错、端口被占这类重启也没用的：写标准错误、退出码 1，核心不再重启 |

握手时核对协议的主版本，对不上照退出码 1 的规矩说清楚再退。能力照实写进清单：没声明的方法调了回 `needs_approval`。

**测**：不用起 Miyu，`FakeCore` 在进程里演核心：

```python
from miyu_ext import FakeCore
core = FakeCore(ext, config={"hello.greeting": "Hi"})
assert core.call_tool("hello_greet", {"name": "Ann"})["blocks"][0]["text"] == "Hi, Ann!"
core.close()
```

这个仓库自己的测试：`python3 -m unittest discover -s python/tests`。

## Rust

crate `miyu-ext`：同步、只依赖 `serde_json`。处理函数里经 `call.core()`（命令、方法、别的处理函数直接收 `&Core`）调核心的方法、读配置，对应 Python 的 `ext.call`、`ext.config`。

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

完整的例子：`rust/examples/hello.rs`（`cargo build --example hello`，程序放进包的 `bin/`，叫 `miyu-hello`，Windows 上是 `miyu-hello.exe`）。`FakeCore::start(ext, config)` 同 Python 的假核心；测试：在 `rust/` 下 `cargo test`。

## 许可证

MIT，见 `LICENSE`。
