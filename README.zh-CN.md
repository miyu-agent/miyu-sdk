<p align="center">
  <a href="README.md">English</a> | 简体中文
</p>

# Miyu 开发包

> <sub>本文档由 Anthropic 的 Claude Fable 5.2 生成。</sub>

Miyu 开发包是两个用来编写 Miyu 扩展的小型库：Python 版（`python/miyu_ext.py`）和 Rust 版（`rust/`，包名是 `miyu-ext`）。两个版本的概念和名字一一对应。

扩展能做什么由协议决定（见核心仓库的 `docs/zh/reference/extensions.md`），开发包不会额外提供能力。开发包把每个扩展都要重复编写的部分封装了起来：握手、按行收发消息、多个调用同时到达时按编号分别回复、取消、配置推送、标准输入读完以后退出，以及退出码 1 的规则。

## Python

Python 版只有一个文件，只使用标准库，需要 Python 3.9 或更高的版本。把它复制到你的软件包的 `bin/` 目录里，和程序放在一起：

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

完整的例子在 `python/examples/hello/` 目录里，清单的写法见核心仓库的 `docs/zh/dev/manifest.md`。

同一个文件可以在三个平台上使用：在 Linux 和 macOS 上，系统按 `#!` 行直接启动程序（程序需要有可执行权限）；在 Windows 上，核心按 `#!` 行查找解释器，其中 `python3` 会查找 `python`（需要核心 0.8.0 或更高的版本）。Miyu 不自带 Python，请在软件包的说明里写明需要安装 Python 3.9 或更高的版本。

| 写法 | 作用 |
|---|---|
| `@ext.tool(name, description, schema, access=…)` | 定义一个工具：函数接收 `(args, call)`，返回文字、`Reply` 或者完整的回答。`call.cancelled` 表示这次调用是否已经超时或者被人打断 |
| `Reply(text, error, data, detail, sent, end_turn, state)` | 完整的回答：`data` 只交给 AI 在命令里用 `miyu call` 调用这个工具的脚本（一行 JSON）；`detail` 只交给界面用来绘制卡片；`sent` 是 AI 发出去的图片；`end_turn` 表示这一轮到此结束；`state` 是这个软件包在这个会话里的一份状态（`{}` 表示清空，`None` 表示没有填写，核心不接收出错的回答里的状态；状态按原样传递） |
| `@ext.tool(…, paths=…, background_parameters=…, runs_commands=…)` | 规格里另外三项：哪些参数是路径（`[{"parameter": "file_path", "write": True}]`）；只在会话可以把任务放到后台时才提供的参数；这个工具会运行命令。详细的见下面的「修改文件、运行命令的工具」 |
| `@ext.tool(…, plugin=…, role=…, scope=…, result=…)` | 规格里的另外几项，不需要时不用写：场所插件（只有在群聊、私聊里打开了这个插件时才提供这个工具）；职能（`read_file`）；在 `venues` 的范围内进一步收窄的会话（`any`、`local`、`local_main`、`local_main_attended`、`spawn`）；`data` 的格式（一行文字）。`access` 还可以取 `install` |
| `Reply(…).changed(path)`、`.read(path, lines)`、`.trashed(path, trash)`、`.todos(todos, done)` | 回答里的 `effects`：修改了、读取了、删除到回收站的文件，以及换上的待办列表。`changed` 可以带上 `before`、`after`（`content_hash(…)`）或者 `before_text`、`after_text`，让核心多核对一次 |
| `call.checked`、`call.channel`、`call.command_env()` | 核心检查过的真实路径（声明了 `paths` 的工具才有）；`miyu call` 的通道（声明了 `runs_commands` 的工具才有），以及已经设置好通道的子进程环境 |
| `@ext.command(name)` | 清单里 `[[slash]]` 定义的一条斜杠命令：函数返回给人看的一段文字；函数运行期间，`ext.language` 是输入这条命令的那个连接所用的语言 |
| `@ext.method(name, timeout_ms=None, later=False)` | 供后台页、界面块的按钮、各个界面、其他软件包（按总线的名称或者软件包编号）调用的方法：函数接收 `(params, caller)`；抛出 `MethodError(code, message)` 会返回带错误码的错误（错误码放在错误的 `data.code` 里，`data.reason` 也是这个错误码）；`timeout_ms` 是核心等待的时间（1000 到 600000，默认 30 秒）；`later=True` 表示先不登记，等调用 `ext.enable_methods([...])` 时再登记（例如等模型下载完成），调用 `ext.disable_methods([...])` 可以撤销登记 |
| `@ext.on(method)` | 其他反向调用和推送：`user.append`、`search.query`、`widget.watch`……；抛出 `MethodError(code, message)` 会返回带错误码的错误，核心读取 `data.reason`（`search.query` 达到限额时返回 `limited`） |
| `@ext.on_config`、`ext.config` | 配置推送；握手时收到的配置在 `ext.config` 里 |
| `@ext.ready` | 握手和登记完成以后，在单独的线程里运行一次 |
| `ext.call(method, params)` | 调用核心的方法（`blob.put`、`model.call`……）；调用被拒绝时抛出 `CoreError`，`reason` 是原因码 |
| `ext.fatal(message)` | 用于配置错误、端口被占用这类重启也解决不了的问题：写入标准错误，以退出码 1 退出，核心不会再重启这个扩展 |

握手时开发包会核对协议的主版本号；如果主版本号对不上，开发包会按退出码 1 的规则说明原因，然后退出。清单里的能力要如实填写：调用没有声明能力的方法会返回 `needs_approval`。

**测试**：测试时不需要启动 Miyu，`FakeCore` 会在进程里模拟核心：

```python
from miyu_ext import FakeCore
core = FakeCore(ext, config={"hello.greeting": "Hi"})
assert core.call_tool("hello_greet", {"name": "Ann"})["blocks"][0]["text"] == "Hi, Ann!"
core.close()
```

假核心会像核心一样检查规格里的三项（写错时返回 `bad_tool`；会运行命令的工具，如果 `approved` 里没有 `tools.commands`，返回 `needs_approval`，默认全部批准），被拒绝的调用记录在 `core.refused` 里。`call_tool` 会像核心一样带上 `checked`（声明的路径按 `core.cwd`、`core.home` 换算成真实位置）和 `channel`（一份假的 `core.channel`，假核心不处理通过它的调用），并像核心一样检查回答里的 `effects`：接受的效果记录在 `core.effects` 里（路径是真实位置），丢弃的效果记录在 `core.dropped` 里，每一条是一个 `{"tool", "kind", "why"}`。

运行这个仓库自己的测试：`python3 -m unittest discover -s python/tests`。

## Rust

Rust 版的包名是 `miyu-ext`。它以同步方式工作，只依赖 `serde_json`。在处理函数里通过 `call.core()` 调用核心的方法、读取配置（命令、方法和其他处理函数直接接收 `&Core`），对应 Python 版的 `ext.call` 和 `ext.config`。

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

完整的例子是 `rust/examples/hello.rs`（用 `cargo build --example hello` 编译，把程序放进软件包的 `bin/` 目录，命名为 `miyu-hello`，在 Windows 上命名为 `miyu-hello.exe`）。`FakeCore::start(ext, config)` 和 Python 版的假核心作用相同；测试方法是在 `rust/` 目录下运行 `cargo test`。

两个版本的名字对应关系如下（下面一节会用到）：

| Python | Rust |
|---|---|
| `@ext.tool(…, paths=[…], background_parameters=[…], runs_commands=True)` | `Tool::paths(json!([…]))`、`.background_parameters(&[…])`、`.runs_commands(true)` |
| `Reply(effects=[…])` | `Reply::effects(vec![…])` |
| `@ext.tool(…, plugin=…, role=…, scope=…, result=…)` | `Tool::plugin(…)`、`.role(…)`、`.scope(…)`、`.result(…)` |
| `Reply(state=…)` | `Reply::state(json!(…))` |
| 在 `@ext.on(method)` 里 `raise MethodError(code, message)` | `ext.on_with(method, …)` 返回 `Err(MethodError::new(code, message))`（不返回错误的 `ext.on` 用法不变） |
| `.changed(path)`、`.changed(path, before=…, after=…, before_text=…, after_text=…)` | `.changed(path)`、`.changed_with(path, Change { before, after, before_text, after_text })` |
| `.read(path, lines=(1, 20))`、`.trashed(path, trash)`、`.todos(todos, done=None)` | `.read(path, Some([1, 20]))`、`.trashed(path, trash)`、`.todos(todos, None)` |
| `content_hash(data)` | `content_hash(&data)` |
| `call.persona`（这一轮的人格编号，没有人格时是 `None`）、`call.read_only`（调用没有带权限时是 `None`，表示不知道，此时要按最严格的情况处理，不要当成可以写入） | `call.persona()`、`call.read_only()`（`Option<bool>`） |
| `call.pictures`（存放图片的目录，只提供给本机的会话，没有时是 `None`）、`call.permission`（完整的 `{"level", "read_only"}`，没有时是 `None`） | `call.pictures()`、`call.permission_level()`（都是 `Option<&str>`） |
| `call.checked`（列表）、`call.channel`（字典或者 `None`） | `call.checked()`（`Vec<&str>`）、`call.channel()`（`Option<Channel>`） |
| `call.command_env()`：完整的环境（`os.environ` 的副本加上通道） | `call.command_env()`：只包含需要添加的几项，用于 `Command::envs` |
| `FakeCore(ext, approved=[…], cwd=…, home=…)`、`core.channel = …` | `FakeCore::start_approved(ext, config, &[…])`、`set_cwd`、`set_home`、`set_channel` |
| `core.refused`、`core.effects`、`core.dropped` | `core.refused()`、`core.effects()`、`core.dropped()`、`clear_effects()` |
| `@ext.method(name, timeout_ms=…, later=True)`、`raise MethodError(code, message)` | `ext.method_with(Method::new(name).timeout_ms(…).later(), …)`、`Err(MethodError::new(code, message))`（只写名字、返回 `Err(String)` 的用法不变） |
| `ext.enable_methods([…])`、`ext.disable_methods([…])` | `core.enable_methods(&[…])`、`core.disable_methods(&[…])` |

## 修改文件、运行命令的工具

核心从提交 `2490fb77`、`514bdc40` 开始支持这两类工具。协议见核心仓库的 `docs/zh/reference/extensions.md`（`provide`、`tool.call` 下的「效果」「跑命令」），写法见 `docs/zh/dev/extensions.md` 的「改文件的工具」「跑命令的工具」。

**修改文件的工具**（编辑器、格式化、改名这类工具）：如果你想让工具和内置的 `edit` 一样显示差异和增删的行数，并且在 AI 读过的文件被你修改以后不需要重新读取，需要做到下面三点。

1. 在规格里写 `paths`，`access` 写 `write`：核心会按它做权限检查（写入工作区以外的位置要先问人，人同意以前你收不到调用）。
2. 只写入 `call.checked` 里的位置：那是检查通过的真实位置，顺序和参数里路径的顺序相同。不要自己拿参数里的路径去写入（检查以后，路径上的链接可能已经被替换）。
3. 在回答里报告 `changed(路径)`：只需要说明是哪个文件，内容由核心自己读取（修改前的内容在发出调用以前读取，修改后的内容在你回答以后读取）。遇到以下几种情况，核心会丢掉那一条效果，结果仍然正常返回：前后内容相同；路径不在 `checked` 里；路径只检查过读取，却报告了修改或删除；种类不认识；格式不对。

```python
@ext.tool("replace_text", "Replace one exact piece of text in a file.", SCHEMA, access="write",
          paths=[{"parameter": "file_path", "write": True}])
def replace(args, call):
    path = call.checked[0]
    ...  # 改 path
    return Reply(text="Replaced.").changed(path)
```

完整的例子是 `python/examples/replace/` 和 `rust/examples/replace.rs`，仓库的测试会用假核心运行它们。

**运行命令的工具**（替换内置的 `shell`、在容器里运行命令这类工具）：在清单的能力里加上 `tools.commands`，在规格里写 `runs_commands=True`。会话里有脚本可以调用的工具时，`call.channel` 是这次调用的通道；启动命令时用 `call.command_env()` 作为环境（它设置好了 `MIYU_CALL_SOCK`、`MIYU_CALL_TOKEN`，并把 `bin` 放在 `PATH` 的最前面），命令里的 `miyu call` 就会以这个会话的身份调用工具。如果命令是你的扩展进程启动的（子孙进程），要等命令运行完再回答，因为你一回答，通道就会关闭。

```python
subprocess.run(command, shell=True, cwd=call.cwd, env=call.command_env())
```

```rust
std::process::Command::new("sh").args(["-c", command]).envs(call.command_env()).status()
```

## 版本

当前版本是 v0.1.0。发布新版本时打 `vX.Y.Z` 标签，同时修改 `python/miyu_ext.py` 里的 `__version__` 和 `rust/Cargo.toml` 里的版本号；两个版本在三个平台上的测试（`.github/workflows/ci.yml`）都通过以后才打标签。Rust 项目按标签添加依赖：

```toml
[dependencies]
miyu-ext = { git = "https://github.com/miyu-agent/miyu-sdk", tag = "v0.1.0" }
```

## 许可证

Apache-2.0，见 `LICENSE`。用这个开发包编写的扩展可以使用任何许可证：因为这些扩展只通过协议和 Miyu 通信，是独立的作品，所以不受 Miyu 核心的 AGPL 约束。
