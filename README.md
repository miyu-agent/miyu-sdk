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
| `@ext.tool(…, paths=…, background_parameters=…, runs_commands=…)` | 规格多的三格：哪几个参数是路径（`[{"parameter": "file_path", "write": True}]`）、只在会话能放后台时才给的参数、这件工具会跑命令。见下面「改文件、跑命令的工具」 |
| `Reply(…).changed(path)`、`.read(path, lines)`、`.trashed(path, trash)`、`.todos(todos, done)` | 回答里的 `effects`：改了、读了、删进回收站的文件，换上的待办。`changed` 可以带 `before`、`after`（`content_hash(…)`）或 `before_text`、`after_text` 多核对一层 |
| `call.checked`、`call.channel`、`call.command_env()` | 核心判过的真实路径（声明了 `paths` 的）；`miyu call` 的通道（声明了 `runs_commands` 的）和设好通道的子进程环境 |
| `@ext.command(name)` | 清单 `[[slash]]` 里的一条斜杠命令：交回给人看的那段话 |
| `@ext.method(name, timeout_ms=None, later=False)` | 后台页、界面块的按钮、头、别的包（照总线的名字或者包编号）调的方法：函数收 `(params, caller)`；抛 `MethodError(code, message)` 回带码的错（码在错误的 `data.code`）；`timeout_ms` 是核心等多久（1000 到 600000，默认 30 秒）；`later=True` 先不登记，等 `ext.enable_methods([...])`（比如模型下好了），`ext.disable_methods([...])` 撤掉 |
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

假核心照核心查规格的三格（写错的 `bad_tool`；会跑命令、`approved` 里没有 `tools.commands` 的 `needs_approval`，默认全批），被拒的在 `core.refused`。`call_tool` 照核心带 `checked`（声明的路径照 `core.cwd`、`core.home` 换成真实位置）和 `channel`（假的一份 `core.channel`，假核心不接），回答里的 `effects` 照核心查：收下的进 `core.effects`（路径是真实位置），丢掉的进 `core.dropped`，一条一个 `{"tool", "kind", "why"}`。

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

两份的名字对照（下面一节用到的）：

| Python | Rust |
|---|---|
| `@ext.tool(…, paths=[…], background_parameters=[…], runs_commands=True)` | `Tool::paths(json!([…]))`、`.background_parameters(&[…])`、`.runs_commands(true)` |
| `Reply(effects=[…])` | `Reply::effects(vec![…])` |
| `.changed(path)`、`.changed(path, before=…, after=…, before_text=…, after_text=…)` | `.changed(path)`、`.changed_with(path, Change { before, after, before_text, after_text })` |
| `.read(path, lines=(1, 20))`、`.trashed(path, trash)`、`.todos(todos, done=None)` | `.read(path, Some([1, 20]))`、`.trashed(path, trash)`、`.todos(todos, None)` |
| `content_hash(data)` | `content_hash(&data)` |
| `call.persona`（这一轮的人格编号，没有是 `None`）、`call.read_only`（核心 TC-1；调用没带权限的是 `None`：不知道，照最严的算，别当成能写） | `call.persona()`、`call.read_only()`（`Option<bool>`） |
| `call.checked`（列表）、`call.channel`（字典或 `None`） | `call.checked()`（`Vec<&str>`）、`call.channel()`（`Option<Channel>`） |
| `call.command_env()`：整份环境（`os.environ` 的副本加上通道） | `call.command_env()`：只是要加的几项，给 `Command::envs` |
| `FakeCore(ext, approved=[…], cwd=…, home=…)`、`core.channel = …` | `FakeCore::start_approved(ext, config, &[…])`、`set_cwd`、`set_home`、`set_channel` |
| `core.refused`、`core.effects`、`core.dropped` | `core.refused()`、`core.effects()`、`core.dropped()`、`clear_effects()` |
| `@ext.method(name, timeout_ms=…, later=True)`、`raise MethodError(code, message)` | `ext.method_with(Method::new(name).timeout_ms(…).later(), …)`、`Err(MethodError::new(code, message))`（只写名字、回 `Err(String)` 的照旧） |
| `ext.enable_methods([…])`、`ext.disable_methods([…])` | `core.enable_methods(&[…])`、`core.disable_methods(&[…])` |

## 改文件、跑命令的工具

核心 XG-1 上、下（2490fb77、514bdc40）起。协议在核心仓库的 `docs/reference/extensions.md`（`provide`、`tool.call` 的「效果」「跑命令」），怎么写见 `docs/authors/extensions.md` 的「改文件的工具」「跑命令的工具」。

**改文件的**（编辑器、格式化、改名这类）：想和内置的 `edit` 一样有差异、增删行数，AI 读过的文件被你改了不用重读。

1. 规格写 `paths`，`access` 写 `write`：核心照它做权限检查（工作区外的写先问人，人点头以前你收不到调用）。
2. 只写 `call.checked` 里的位置：那是检查判过的真实位置，照参数里路径的先后。别自己拿参数里的路径去写（检查以后路径上的链接可能被换过）。
3. 回答里报 `changed(路径)`：只说哪个文件，内容核心自己读（改前在发调用以前、改后在你回答以后）。前后一样的、路径不在 `checked` 里的、只判了读却说改了删了的、不认识的种类、写法不对的，核心丢掉那一条，结果照常。

```python
@ext.tool("replace_text", "Replace one exact piece of text in a file.", SCHEMA, access="write",
          paths=[{"parameter": "file_path", "write": True}])
def replace(args, call):
    path = call.checked[0]
    ...  # 改 path
    return Reply(text="Replaced.").changed(path)
```

完整的例子：`python/examples/replace/`、`rust/examples/replace.rs`，测试照假核心跑它们。

**跑命令的**（换掉内置 `shell`、在容器里跑）：清单的能力加 `tools.commands`，规格写 `runs_commands=True`。会话里有脚本能调的工具时，`call.channel` 是这次调用的通道；起命令时用 `call.command_env()` 当环境（设好 `MIYU_CALL_SOCK`、`MIYU_CALL_TOKEN`，`bin` 放 `PATH` 最前），命令里的 `miyu call` 就以这个会话的身份调工具。命令要是你的扩展进程起的（子孙进程），等它跑完再回答：一回答通道就关了。

```python
subprocess.run(command, shell=True, cwd=call.cwd, env=call.command_env())
```

```rust
std::process::Command::new("sh").args(["-c", command]).envs(call.command_env()).status()
```

## 版本

现在是 v0.1.0。发版打 `vX.Y.Z` 标签，`python/miyu_ext.py` 的 `__version__`、`rust/Cargo.toml` 的版本跟着改；两份在三个平台上的测试（`.github/workflows/ci.yml`）过了才打。Rust 照标签依赖：

```toml
[dependencies]
miyu-ext = { git = "https://github.com/miyu-agent/miyu-sdk", tag = "v0.1.0" }
```

## 许可证

Apache-2.0，见 `LICENSE`（2026-10-26 起，以前是 MIT）。照着写的扩展用什么许可证都行：它们只经协议和 Miyu 说话，是独立的作品，Miyu 核心的 AGPL 管不到它们。
