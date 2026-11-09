"""Write a Miyu extension in Python: the handshake, line-by-line JSON-RPC, concurrent calls, cancelling and config pushes
are handled here, so an extension is a few decorated functions.

One file, standard library only, Python 3.9 or newer. Copy it next to your program in the package's `bin/` folder, or
install it with pip. Apache-2.0 licensed. The protocol itself is in Miyu's `docs/reference/extensions.md`.

    from miyu_ext import Extension

    ext = Extension("hello", "0.1.0")

    @ext.tool("hello_greet", "Greet someone by name.",
              {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]},
              access="read", display_name={"en": "Greet", "zh": "打招呼"}, subject="name")
    def greet(args, call):
        return f"{ext.config.get('hello.greeting', 'Hello')}, {args['name']}!"

    ext.run()
"""

import hashlib
import json
import os
import queue
import re
import stat
import sys
import tempfile
import threading
import time
import traceback

__version__ = "0.1.0"

#: The protocol major versions this file speaks.
PROTOCOL = (1, 1)

#: Exit code for "restarting won't help" (bad config, port taken, protocol mismatch): the core stops instead of retrying.
EXIT_CONFIG = 1

_METHOD_NOT_FOUND = -32601
_INTERNAL_ERROR = -32603
_MIYU_ERROR = -32010
#: A method's error with a code (`MethodError`): JSON-RPC's server-error range, the code in `data.code`.
_CODED_ERROR = -32000


def content_hash(data):
    """The protocol's content hash of `data` (bytes): `sha256:` and 64 lowercase hex digits, as `Reply.changed` takes it."""
    return "sha256:" + hashlib.sha256(data).hexdigest()


class CoreError(Exception):
    """The core refused a request: `reason` is the stable code (`data.reason`), `message` is for people."""

    def __init__(self, error):
        self.code = error.get("code")
        self.data = error.get("data") or {}
        self.reason = self.data.get("reason")
        self.message = error.get("message", "")
        super().__init__(f"{self.reason or self.code}: {self.message}")


class MethodError(Exception):
    """Raise it from a method to answer with an error callers can tell apart: `code` (`bad_audio`, `mic_busy`) goes back
    in the JSON-RPC error's `data.code`, `message` is for people. Any other exception goes back without a code."""

    def __init__(self, code, message):
        self.code = code
        self.message = message
        super().__init__(f"{code}: {message}")


class Reply:
    """A full tool result. Returning a string from a tool is the same as `Reply(text=...)`.

    `blocks` are what the model sees; `error` marks a failure (explain it in the text). `data` goes only to scripts that call
    the tool with `miyu call`; `detail` only to the interfaces (links, table or images shape); `sent` are images she sends;
    `end_turn` ends the turn after this tool. `effects` say which files the call read, changed or trashed and the to-do
    list it wrote: add them with `changed`, `read`, `trashed` and `todos`, which return the reply so they chain.

        return Reply(text="Edited a.txt.").changed(call.checked[0])
    """

    def __init__(self, text=None, blocks=None, error=False, data=None, detail=None, sent=None, end_turn=False,
                 effects=None):
        self.blocks = list(blocks or [])
        if text is not None:
            self.blocks.insert(0, {"type": "text", "text": text})
        self.error = error
        self.data = data
        self.detail = detail
        self.sent = sent
        self.end_turn = end_turn
        self.effects = list(effects or [])

    def changed(self, path, before=None, after=None, before_text=None, after_text=None):
        """The file at `path` was changed or created (`file.changed`). The core reads it itself, before the call and after
        your answer, so the path is enough. `before`, `after` (`content_hash`) or `before_text`, `after_text` are an
        extra check: when they don't match what the core read, it drops the effect. The path must be one of
        `call.checked` from a parameter declared in `paths` with `write`."""
        effect = {"kind": "file.changed", "path": path}
        for key, value in (("before", before), ("after", after), ("before_text", before_text),
                           ("after_text", after_text)):
            if value is not None:
                effect[key] = value
        self.effects.append(effect)
        return self

    def read(self, path, lines=None):
        """The call read the file at `path` (`file.read`); `lines` is `(first, last)`, counting from 1. The path must be
        one of `call.checked`."""
        effect = {"kind": "file.read", "path": path}
        if lines is not None:
            effect["lines"] = list(lines)
        self.effects.append(effect)
        return self

    def trashed(self, path, trash):
        """The file at `path` went to the trash, now at `trash` (shown to people only, at most 4096 characters). The path
        must be one of `call.checked`, declared with `write`, and gone by the time you answer."""
        self.effects.append({"kind": "file.trashed", "path": path, "trash": trash})
        return self

    def todos(self, todos, done=None):
        """The call replaced the to-do list (`todo.written`, as the built-in `todowrite`): `todos` is
        `[{"content", "status"}]`, `done` the finished list it cleared. Not kept when the reply is an error."""
        effect = {"kind": "todo.written", "todos": list(todos)}
        if done is not None:
            effect["done"] = list(done)
        self.effects.append(effect)
        return self

    def to_json(self):
        out = {"blocks": self.blocks}
        for key in ("data", "detail", "sent"):
            value = getattr(self, key)
            if value is not None:
                out[key] = value
        if self.error:
            out["error"] = True
        if self.end_turn:
            out["end_turn"] = True
        if self.effects:
            out["effects"] = self.effects
        return out


class Call:
    """One `tool.call`: the arguments (already repaired to the schema), who asked and where. Check `cancelled` in long work:
    it turns true when the call timed out or the person interrupted.

    `checked` (tools that declare `paths`): the real locations the core's permission check allowed, in the order the paths
    appear in the arguments. Write only these: a link on the path may have been swapped since the check. `channel` (tools
    that declare `runs_commands`, when the session has one): `{"sock", "token", "bin"?}` for `miyu call` in the commands
    you start; `command_env()` turns it into their environment.
    """

    def __init__(self, params):
        self.params = params
        self.session = params.get("session")
        self.call_id = params.get("call_id")
        self.tool = params.get("tool")
        self.cwd = params.get("cwd")
        self.by = params.get("by")
        self.owner = params.get("owner", False)
        self.checked = list(params.get("checked") or [])
        self.channel = params.get("channel")
        self._cancelled = threading.Event()

    @property
    def cancelled(self):
        return self._cancelled.is_set()

    def command_env(self, env=None):
        """The environment for a command you start for this call: a copy of `env` (default `os.environ`) with
        `MIYU_CALL_SOCK`, `MIYU_CALL_TOKEN` set and `bin` first on `PATH`, so `miyu call` in it reaches this session.
        Without a channel it is a plain copy. Start the command yourself (a child or grandchild of this process) and
        answer after it ends: the channel closes with the answer.

            subprocess.run(command, shell=True, cwd=call.cwd, env=call.command_env())
        """
        env = dict(os.environ if env is None else env)
        channel = self.channel
        if not channel:
            return env
        env["MIYU_CALL_SOCK"] = channel["sock"]
        env["MIYU_CALL_TOKEN"] = channel["token"]
        if channel.get("bin"):
            # Windows spells it `Path` too: keep whichever key is there.
            key = next((name for name in env if os.name == "nt" and name.upper() == "PATH"), "PATH")
            old = env.get(key)
            env[key] = channel["bin"] + (os.pathsep + old if old else "")
        return env


class Extension:
    """An extension: register tools, commands, methods and handlers, then `run()`.

    `kind` and `version` go into the handshake (`head`), for the core's log. After the handshake `config` holds this
    package's settings (secrets as their values), `language` the language for people (`zh`, `en`, `ja`) and `account` who
    this extension is.
    """

    def __init__(self, kind, version="0.0.0"):
        self.kind = kind
        self.version = version
        self.config = {}
        self.language = "en"
        self.account = None
        self._tools = []
        self._tool_handlers = {}
        self._commands = {}
        self._methods = {}
        self._method_specs = {}
        self._methods_lock = threading.Lock()
        self._handlers = {}
        self._config_handlers = []
        # Config pushes and other notifications are handled on one thread of their own, in order: a handler may call the
        # core and wait, which it couldn't on the reading thread (the answer would never be read).
        self._notices = None
        self._notices_lock = threading.Lock()
        self._ready_handlers = []
        self._out = None
        self._write_lock = threading.Lock()
        self._waiting = {}
        self._waiting_lock = threading.Lock()
        self._next_id = 0
        # Starts every request id: this process and when it started. The core answers a request whose id it saw before
        # in the same session with the earlier answer and does nothing, across connections; plain x1, x2... would make a
        # restarted extension's first session.send calls silently replay old ones.
        self._id_prefix = f"x{os.getpid():x}{time.time_ns():x}-"
        self._calls = {}

    # Registering ---------------------------------------------------------------------------------------------------

    def tool(self, name, description, schema=None, *, access, venues=("local",), timeout_ms=None, display_name=None,
             subject=None, need_load=None, clicall=None, feature=None, paths=None, background_parameters=None,
             runs_commands=None):
        """Register a tool. The function gets `(args, call)` and returns a string, a `Reply` or a raw result dict.

        `description` and the schema are for the model: short English sentences, at most three. `access` is honest:
        `read`, `write`, `execute`, `network`, `outbound` or `venue`.

        `paths` names the parameters that are file paths, `[{"parameter": "file_path", "write": True}]` (top-level string
        or string-array parameters; `write` needs `access="write"`): the core checks them like the built-in `edit`, hands
        the allowed locations over as `call.checked`, and only then keeps the file effects you report.
        `background_parameters` are parameters that only make sense when the session can run things in the background
        (not required ones). `runs_commands=True` says the tool runs commands: the package needs the `tools.commands`
        capability, and `call.channel` lets `miyu call` in those commands reach the session.
        """
        spec = {
            "name": name,
            "description": description,
            "input_schema": schema or {"type": "object", "properties": {}},
            "access": access,
            "venues": list(venues),
        }
        for key, value in (("timeout_ms", timeout_ms), ("display_name", display_name), ("subject", subject),
                           ("need_load", need_load), ("clicall", clicall), ("feature", feature),
                           ("paths", paths), ("background_parameters", background_parameters),
                           ("runs_commands", runs_commands)):
            if value is not None:
                spec[key] = value

        def register(function):
            self._tools.append(spec)
            self._tool_handlers[name] = function
            return function

        return register

    def command(self, name):
        """Handle the slash command `name` (declared in the manifest's `[[slash]]`). The function gets the `command.call`
        params and returns the text shown to the person."""

        def register(function):
            self._commands[name] = function
            return function

        return register

    def method(self, name, *, timeout_ms=None, later=False):
        """Handle the method `name` called from your page, a widget button, a head or another package (`method.call`;
        a bus name's methods are written in full, `stt.transcribe`). The function gets `(params, caller)`: `caller` is
        the calling package's id, or `None` for a person; raise `MethodError` for an error with a code. `timeout_ms`
        (1000 to 600000) is how long the core waits, by default 30 seconds. `later=True` leaves it unregistered until
        `enable_methods` names it, for example once a model has downloaded."""

        def register(function):
            self._methods[name] = function
            self._method_specs[name] = {"timeout_ms": timeout_ms, "on": not later}
            return function

        return register

    def enable_methods(self, names):
        """Register the `later` methods named, with the ones already registered (`package.methods` replaces the whole
        list, so it is sent in full). Raises `ValueError` for a name never declared (nothing is sent then), `CoreError`
        when the core refuses."""
        self._switch_methods(names, True)

    def disable_methods(self, names):
        """Unregister the methods named, keeping the rest, until `enable_methods` names them again. Raises like
        `enable_methods`."""
        self._switch_methods(names, False)

    def _switch_methods(self, names, on):
        # Held while sending, so switches from two threads reach the core in the order they changed the list.
        with self._methods_lock:
            unknown = [name for name in names if name not in self._method_specs]
            if unknown:
                raise ValueError(f"never declared: {', '.join(unknown)}")
            for name in names:
                self._method_specs[name]["on"] = on
            self.call("package.methods", self._methods_params())

    def _methods_params(self):
        methods = []
        for name in sorted(self._method_specs):
            spec = self._method_specs[name]
            if spec["on"]:
                entry = {"name": name}
                if spec["timeout_ms"] is not None:
                    entry["timeout_ms"] = spec["timeout_ms"]
                methods.append(entry)
        return {"methods": methods}

    def on(self, method):
        """Handle any other request or notification from the core, such as `user.append`, `search.query` or
        `widget.watch`. The function gets the params; for a request, what it returns is the answer."""

        def register(function):
            self._handlers[method] = function
            return function

        return register

    def on_config(self, function):
        """Call `function(changed)` after a config push; `changed` maps keys to new values (`None` when unset). Handlers run
        on one thread of their own, in the order the pushes came, so they may call the core; `config` already has the new
        values when a request after the push arrives."""
        self._config_handlers.append(function)
        return function

    def ready(self, function):
        """Call `function()` on its own thread once the handshake and registration are done: a place for work that calls
        the core."""
        self._ready_handlers.append(function)
        return function

    # Talking to the core -------------------------------------------------------------------------------------------

    def call(self, method, params=None, timeout=90.0):
        """Call a core method and wait for its result. Raises `CoreError` when refused, `TimeoutError` when it doesn't
        answer in time. Only from handlers or threads started after `run()` began: the answer arrives through `run()`."""
        with self._waiting_lock:
            self._next_id += 1
            request_id = f"{self._id_prefix}{self._next_id}"
            box = {"event": threading.Event()}
            self._waiting[request_id] = box
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}})
        if not box["event"].wait(timeout):
            with self._waiting_lock:
                self._waiting.pop(request_id, None)
            raise TimeoutError(f"{method} had no answer in {timeout} seconds")
        message = box["message"]
        if "error" in message:
            raise CoreError(message["error"])
        return message.get("result")

    def fatal(self, message):
        """Say why on stderr and exit with code 1, which tells the core not to restart: use it for wrong settings and
        taken ports."""
        print(message, file=sys.stderr, flush=True)
        sys.exit(EXIT_CONFIG)

    # Running -------------------------------------------------------------------------------------------------------

    def run(self, stdin=None, stdout=None):
        """Shake hands, register, then serve until stdin ends (the core asking you to stop). Returns when it ends."""
        stdin = stdin or sys.stdin
        self._out = stdout or sys.stdout
        hello = {"protocol": list(PROTOCOL), "head": {"kind": self.kind, "version": self.version}}
        self._send({"jsonrpc": "2.0", "id": "hello", "method": "hello", "params": hello})
        shaken = False
        for line in stdin:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                print(f"miyu_ext: unreadable line from the core: {line[:200]}", file=sys.stderr, flush=True)
                continue
            if not shaken:
                if message.get("id") != "hello":
                    continue
                self._shake(message)
                shaken = True
                threading.Thread(target=self._register, daemon=True).start()
                continue
            self._dispatch(message)

    def _shake(self, message):
        if "error" in message:
            error = CoreError(message["error"])
            if error.reason == "protocol_mismatch":
                self.fatal(f"miyu_ext {__version__} speaks protocol {PROTOCOL[0]}-{PROTOCOL[1]}, which this Miyu doesn't: "
                           f"use a matching miyu_ext ({error.message})")
            self.fatal(f"the handshake was refused: {error}")
        result = message.get("result", {})
        major = result.get("protocol")
        if not (isinstance(major, int) and PROTOCOL[0] <= major <= PROTOCOL[1]):
            self.fatal(f"miyu_ext {__version__} speaks protocol {PROTOCOL[0]}-{PROTOCOL[1]}, the core chose {major}")
        self.config = dict(result.get("config", {}))
        self.language = result.get("language", "en")
        self.account = result.get("account")

    def _register(self):
        try:
            self.call("provide", {"tools": self._tools})
            if self._methods:
                with self._methods_lock:
                    self.call("package.methods", self._methods_params())
        except (CoreError, TimeoutError) as error:
            print(f"miyu_ext: registering failed: {error}", file=sys.stderr, flush=True)
            return
        for function in self._ready_handlers:
            threading.Thread(target=function, daemon=True).start()

    def _dispatch(self, message):
        if "method" not in message:
            with self._waiting_lock:
                box = self._waiting.pop(message.get("id"), None)
            if box is not None:
                box["message"] = message
                box["event"].set()
            return
        method = message["method"]
        params = message.get("params") or {}
        if "id" not in message:
            self._notified(method, params)
            return
        if method == "tool.call":
            call = Call(params)
            self._calls[call.call_id] = call
        threading.Thread(target=self._answer, args=(message["id"], method, params), daemon=True).start()

    def _notified(self, method, params):
        if method == "extension.config":
            changed = params.get("keys", {})
            for key, value in changed.items():
                if value is None:
                    self.config.pop(key, None)
                else:
                    self.config[key] = value
            for function in self._config_handlers:
                self._queue(function, changed)
        elif method == "tool.cancel":
            call = self._calls.get(params.get("call_id"))
            if call is not None:
                call._cancelled.set()
        elif method in self._handlers:
            self._queue(self._handlers[method], params)

    def _queue(self, function, argument):
        with self._notices_lock:
            if self._notices is None:
                self._notices = queue.Queue()
                threading.Thread(target=self._run_notices, daemon=True).start()
        self._notices.put((function, argument))

    def _run_notices(self):
        while True:
            function, argument = self._notices.get()
            try:
                function(argument)
            except Exception:  # a handler's bug: report it, keep handling
                traceback.print_exc(file=sys.stderr)

    def _answer(self, request_id, method, params):
        try:
            result = self._handle(method, params)
        except _NotFound:
            self._send({"jsonrpc": "2.0", "id": request_id,
                        "error": {"code": _METHOD_NOT_FOUND, "message": f"no handler for {method}"}})
            return
        except MethodError as error:
            self._send({"jsonrpc": "2.0", "id": request_id,
                        "error": {"code": _CODED_ERROR, "message": error.message, "data": {"code": error.code}}})
            return
        except Exception as error:  # a handler's bug: report it, keep serving
            traceback.print_exc(file=sys.stderr)
            if method == "tool.call":
                result = Reply(text=f"{type(error).__name__}: {error}", error=True).to_json()
            else:
                self._send({"jsonrpc": "2.0", "id": request_id,
                            "error": {"code": _INTERNAL_ERROR, "message": f"{type(error).__name__}: {error}"}})
                return
        finally:
            if method == "tool.call":
                self._calls.pop(params.get("call_id"), None)
        self._send({"jsonrpc": "2.0", "id": request_id, "result": result})

    def _handle(self, method, params):
        if method == "tool.call":
            function = self._tool_handlers.get(params.get("tool"))
            if function is None:
                raise _NotFound()
            returned = function(params.get("args", {}), self._calls[params.get("call_id")])
            if isinstance(returned, Reply):
                return returned.to_json()
            if isinstance(returned, dict):
                return returned
            return Reply(text=str(returned)).to_json()
        if method == "command.call":
            function = self._commands.get(params.get("command"))
            if function is None:
                raise _NotFound()
            return {"said": str(function(params))}
        if method == "method.call":
            function = self._methods.get(params.get("method"))
            if function is None:
                raise _NotFound()
            return function(params.get("params", {}), params.get("from"))
        function = self._handlers.get(method)
        if function is None:
            raise _NotFound()
        return function(params)

    def _send(self, message):
        line = json.dumps(message, ensure_ascii=False, separators=(",", ":"))
        with self._write_lock:
            self._out.write(line + "\n")
            self._out.flush()


class _NotFound(Exception):
    pass


class FakeCore:
    """A core in the same process, for testing an extension without Miyu: it shakes hands (handing over `config`), answers
    `provide`, `package.methods` and whatever you add to `answers` (method to a function of the params), and lets you
    call tools, commands and methods and push notifications. `close()` ends stdin, so `run()` returns.

        core = FakeCore(ext, config={"hello.greeting": "Hi"})
        assert core.call_tool("hello_greet", {"name": "Ann"})["blocks"][0]["text"] == "Hi, Ann!"
        core.close()

    Like the core, it checks what `provide` declares (`paths`, `background_parameters`, `runs_commands`): a wrong one is
    refused with `bad_tool`, and a tool that runs commands without `tools.commands` among `approved` (default: every
    capability approved) with `needs_approval`; the refusal is in `refused`. `call_tool` sends `checked` (the declared
    paths resolved against `cwd` and `home`, following links) and `channel` (a fake one, `channel`; the fake core doesn't
    serve it) the way the core does, then vets the reply's `effects` as the core does: the ones it keeps are in `effects`
    (paths as real locations), the ones it drops in `dropped` as `{"tool", "kind", "why"}`.
    """

    def __init__(self, ext, config=None, language="en", account="admin", protocol=PROTOCOL[0], answers=None,
                 approved=None, cwd="/", home=None):
        self.received = []
        self.tools = []
        self.methods = []
        self.refused = None
        self.effects = []
        self.dropped = []
        self.cwd = cwd
        self.home = home if home is not None else os.path.expanduser("~")
        self.channel = {"sock": os.path.join(tempfile.gettempdir(), "miyu-call-fake.sock"), "token": "0" * 32,
                        "bin": os.path.join(tempfile.gettempdir(), "miyu-call-fake-bin")}
        self.approved = None if approved is None else list(approved)
        self.answers = {"provide": self._provided, "package.methods": self._methods_listed}
        self.answers.update(answers or {})
        self._hello = {"protocol": protocol, "config": dict(config or {}), "language": language, "account": account,
                       "core": {"version": "0.0.0"}}
        self._lock = threading.Lock()
        self._waiting = {}
        self._next = 0
        self._registered = threading.Event()
        # An extension with methods has registered once `package.methods` arrives too.
        self._expects_methods = bool(ext._methods)
        into_ext, self._to_ext = os.pipe()
        self._from_ext, out_of_ext = os.pipe()
        self._writer = os.fdopen(self._to_ext, "w", encoding="utf-8")
        self._reader = os.fdopen(self._from_ext, "r", encoding="utf-8")
        self._ext_in = os.fdopen(into_ext, "r", encoding="utf-8")
        self._ext_out = os.fdopen(out_of_ext, "w", encoding="utf-8")
        self.extension = threading.Thread(target=ext.run, args=(self._ext_in, self._ext_out), daemon=True)
        self.extension.start()
        self._reading = threading.Thread(target=self._read_all, daemon=True)
        self._reading.start()
        if not self._registered.wait(10):
            raise TimeoutError("the extension didn't shake hands and provide within 10 seconds")

    def call_tool(self, tool, args=None, session="s1", call_id=None, owner=True, timeout=10.0):
        """Call `tool` the way the core does; returns the result dict (or the JSON-RPC error as `{"error": ...}`). The
        result comes back as the extension wrote it; the effects the core would keep land in `effects`, the rest in
        `dropped`."""
        call_id = call_id or f"c{self._next + 1}"
        args = args or {}
        params = {"session": session, "call_id": call_id, "tool": tool, "args": args, "cwd": self.cwd, "owner": owner}
        spec = next((one for one in self.tools if one.get("name") == tool), {})
        judged = []
        if spec.get("paths") is not None:
            judged = self._judge(spec["paths"], args)
            params["checked"] = [real for real, _ in judged]
        if spec.get("runs_commands") is True and self.channel is not None:
            params["channel"] = dict(self.channel)
        befores = {real: _copy(real) for real, write in judged if write}
        message = self._exchange("tool.call", params, timeout)
        if "result" not in message:
            return {"error": message["error"]}
        result = message["result"]
        if isinstance(result, dict):
            self._vet(tool, result, judged, befores)
        return result

    def request(self, method, params, timeout=10.0):
        """Send any core-to-extension request (`command.call`, `method.call`, `user.append`...) and wait for the answer."""
        message = self._exchange(method, params, timeout)
        return message["result"] if "result" in message else {"error": message["error"]}

    def notify(self, method, params):
        """Push a notification (`extension.config`, `tool.cancel`, `widget.watch`...)."""
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def close(self, timeout=10.0):
        """End the extension's stdin and wait for `run()` to return."""
        with self._lock:
            self._writer.close()
        self.extension.join(timeout)
        self._ext_out.close()
        self._reading.join(timeout)
        self._reader.close()
        self._ext_in.close()

    def _exchange(self, method, params, timeout):
        with self._lock:
            self._next += 1
            request_id = f"core-{self._next}"
            box = {"event": threading.Event()}
            self._waiting[request_id] = box
        self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        if not box["event"].wait(timeout):
            raise TimeoutError(f"no answer to {method} within {timeout} seconds")
        return box["message"]

    def _provided(self, params):
        tools = params.get("tools", [])
        for spec in tools:
            problem = _declared_problem(spec)
            if problem is not None:
                name = spec.get("name")
                raise _Refused({"code": _MIYU_ERROR, "message": f"tool {name}: {problem} is wrong",
                                "data": {"reason": "bad_tool", "tool": name, "problem": problem}})
        runs = any(spec.get("runs_commands") is True for spec in tools)
        if runs and self.approved is not None and "tools.commands" not in self.approved:
            raise _Refused({"code": _MIYU_ERROR, "message": "the tools.commands capability is not approved",
                            "data": {"reason": "needs_approval", "capabilities": ["tools.commands"]}})
        self.tools = tools
        if not self._expects_methods:
            self._registered.set()
        return {"tools": len(self.tools)}

    def _methods_listed(self, params):
        self.methods = params.get("methods", [])
        self._registered.set()
        return {}

    # The core's checks on paths and effects (its design 63, sections 3 and 4) ----------------------------------------

    def _real(self, path):
        """Where `path` really is: relative to `cwd`, `~` to `home`, links followed; `None` when it can't be told."""
        rest = _tilde(path)
        joined = os.path.join(self.home, rest) if rest is not None else os.path.join(self.cwd, path)
        try:
            return os.path.realpath(joined)
        except (OSError, ValueError):
            return None

    def _judge(self, declared, args):
        """`(real, write)` for each declared path in `args`, in order; a missing or non-string argument gives none."""
        judged = []
        for one in declared:
            value = args.get(one["parameter"])
            if isinstance(value, str):
                paths = [value]
            elif isinstance(value, list):
                paths = [item for item in value if isinstance(item, str)]
            else:
                paths = []
            for path in paths:
                real = self._real(path)
                if real is not None:
                    judged.append((real, bool(one.get("write"))))
        return judged

    def _vet(self, tool, result, judged, befores):
        raw = result.get("effects")
        if raw is None:
            return
        if not isinstance(raw, list):
            self._drop(tool, "-", "shape")
            return
        error = result.get("error") is True
        afters = {}
        reported = set()
        for item in raw:
            kind = item.get("kind") if isinstance(item, dict) else None
            kind = kind if isinstance(kind, str) else "-"
            if kind == "todo.written" and error:
                continue
            why, effect = self._one(item, judged, befores, afters)
            if why is None:
                key = (kind, effect.get("path"))
                if key[1] is not None and key in reported:
                    why = "shape"
                else:
                    reported.add(key)
                    self.effects.append(effect)
                    continue
            self._drop(tool, kind, why)

    def _drop(self, tool, kind, why):
        self.dropped.append({"tool": tool, "kind": kind, "why": why})

    def _one(self, item, judged, befores, afters):
        """`(None, effect)` when the core keeps `item`, else `(why, None)`."""
        if not isinstance(item, dict) or not isinstance(item.get("kind"), str):
            return "shape", None
        kind = item["kind"]
        if kind not in ("file.changed", "file.read", "file.trashed", "todo.written"):
            return "kind", None
        if kind == "todo.written":
            if not _todo_list(item.get("todos")) or ("done" in item and not _todo_list(item["done"])):
                return "shape", None
            effect = {"kind": kind, "todos": item["todos"]}
            if item.get("done"):
                effect["done"] = item["done"]
            return None, effect
        path = item.get("path")
        if not isinstance(path, str):
            return "shape", None
        if kind == "file.changed":
            for key in ("before", "after"):
                if item.get(key) is not None and not (isinstance(item[key], str) and _HASH.match(item[key])):
                    return "shape", None
            for key in ("before_text", "after_text"):
                if item.get(key) is not None and not isinstance(item[key], str):
                    return "shape", None
            real = self._checked(path, judged, True)
            if real is None:
                return "not_checked", None
            before = befores.get(real, _UNREADABLE)
            if before is _TOO_BIG:
                return "too_big", None
            if before is _UNREADABLE:
                return "mismatch", None
            after = afters.setdefault(real, _copy(real))
            if after is _TOO_BIG:
                return "too_big", None
            if not isinstance(after, bytes):
                return "mismatch", None
            if before == after:
                return "unchanged", None
            if not _agrees(item, before, after):
                return "mismatch", None
            return None, {"kind": kind, "path": real}
        if kind == "file.read":
            lines = item.get("lines")
            if lines is not None:
                if not (isinstance(lines, list) and len(lines) == 2
                        and all(isinstance(n, int) and not isinstance(n, bool) and n >= 0 for n in lines)):
                    return "shape", None
                if lines[0] == 0 or lines[0] > lines[1]:
                    return "shape", None
            real = self._checked(path, judged, False)
            if real is None:
                return "not_checked", None
            now = afters.setdefault(real, _copy(real))
            if now is _TOO_BIG:
                return "too_big", None
            if not isinstance(now, bytes):
                return "mismatch", None
            effect = {"kind": kind, "path": real}
            if lines is not None:
                effect["lines"] = lines
            return None, effect
        trash = item.get("trash")
        if not isinstance(trash, str) or len(trash) > _MOST_TRASH_CHARS:
            return "shape", None
        real = self._checked(path, judged, True)
        if real is None:
            return "not_checked", None
        if os.path.lexists(real):
            return "mismatch", None
        return None, {"kind": kind, "path": real, "trash": trash}

    def _checked(self, path, judged, write):
        """The real location of `path` when the check allowed it (for writing, when `write`), else `None`."""
        real = self._real(path)
        for allowed, allowed_write in judged:
            if allowed == real and (allowed_write or not write):
                return real
        return None

    def _read_all(self):
        for line in self._reader:
            message = json.loads(line)
            self.received.append(message)
            if message.get("method") == "hello":
                self._write({"jsonrpc": "2.0", "id": message["id"], "result": self._hello})
            elif "method" in message:
                self._answer(message)
            else:
                with self._lock:
                    box = self._waiting.pop(message.get("id"), None)
                if box is not None:
                    box["message"] = message
                    box["event"].set()

    def _answer(self, message):
        function = self.answers.get(message["method"])
        if function is None:
            reply = {"error": {"code": _METHOD_NOT_FOUND, "message": "not in FakeCore.answers",
                               "data": {"reason": "unknown_method"}}}
        else:
            try:
                reply = {"result": function(message.get("params", {}))}
            except _Refused as refused:
                reply = {"error": refused.error}
                if message["method"] == "provide":
                    self.refused = refused.error
                    # Refused: no `package.methods` follows, registering is over.
                    self._registered.set()
        self._write({"jsonrpc": "2.0", "id": message["id"], **reply})

    def _write(self, message):
        with self._lock:
            self._writer.write(json.dumps(message, ensure_ascii=False) + "\n")
            self._writer.flush()


class _Refused(Exception):
    """The fake core refuses a request with this JSON-RPC `error`."""

    def __init__(self, error):
        super().__init__(error.get("message"))
        self.error = error


#: The core reads at most this much of a file for the effects; bigger ones are dropped as `too_big`.
_MOST_BYTES = 20 * 1024 * 1024
_MOST_TRASH_CHARS = 4096
_HASH = re.compile(r"sha256:[0-9a-f]{64}\Z")
#: What `_copy` gives for a file over `_MOST_BYTES` and for one that isn't a readable regular file; a missing one is `None`.
_TOO_BIG = object()
_UNREADABLE = object()


def _tilde(path):
    """What follows `~` and a separator (or `~` alone), the way the core reads a home-relative path; `None` otherwise."""
    if path == "~":
        return ""
    for separator in ("/", "\\") if os.name == "nt" else ("/",):
        if path.startswith("~" + separator):
            return path[2:]
    return None


def _copy(real):
    """The file at `real` now: its bytes, `None` when missing, `_TOO_BIG` or `_UNREADABLE`."""
    try:
        status = os.stat(real)
    except FileNotFoundError:
        return None
    except OSError:
        return _UNREADABLE
    if not stat.S_ISREG(status.st_mode):
        return _UNREADABLE
    try:
        with open(real, "rb") as file:
            data = file.read(_MOST_BYTES + 1)
    except OSError:
        return _UNREADABLE
    return _TOO_BIG if len(data) > _MOST_BYTES else data


def _agrees(item, before, after):
    """The hashes and texts `item` carries (if any) match what was read; `before` is `None` for a new file."""
    for key, content in (("before", before), ("after", after)):
        claimed = item.get(key)
        if claimed is not None and (content is None or content_hash(content) != claimed):
            return False
    for key, content in (("before_text", before), ("after_text", after)):
        claimed = item.get(key)
        if claimed is not None and content != claimed.encode("utf-8"):
            return False
    return True


def _todo_list(todos):
    return isinstance(todos, list) and all(
        isinstance(todo, dict) and isinstance(todo.get("content"), str) and isinstance(todo.get("status"), str)
        for todo in todos)


def _declared_problem(spec):
    """The `bad_tool` problem in what a tool spec declares about paths, background parameters and commands, or `None`."""
    schema = spec.get("input_schema") or {}
    properties = schema.get("properties") if isinstance(schema, dict) else None
    properties = properties if isinstance(properties, dict) else {}
    paths = spec.get("paths")
    if paths is not None:
        if not isinstance(paths, list):
            return "paths"
        names = set()
        for one in paths:
            if not isinstance(one, dict) or not set(one) <= {"parameter", "write"}:
                return "paths"
            name, write = one.get("parameter"), one.get("write", False)
            if not isinstance(name, str) or not isinstance(write, bool) or name in names:
                return "paths"
            names.add(name)
            prop = properties.get(name)
            textual = isinstance(prop, dict) and (prop.get("type") == "string" or (
                prop.get("type") == "array" and isinstance(prop.get("items"), dict)
                and prop["items"].get("type") == "string"))
            if not textual or (write and spec.get("access") != "write"):
                return "paths"
    background = spec.get("background_parameters")
    if background is not None:
        required = schema.get("required") if isinstance(schema, dict) else None
        required = required if isinstance(required, list) else []
        if not isinstance(background, list) or not all(isinstance(name, str) for name in background) \
                or len(set(background)) != len(background) \
                or any(name not in properties or name in required for name in background):
            return "background_parameters"
    runs = spec.get("runs_commands")
    if runs is not None and not isinstance(runs, bool):
        return "runs_commands"
    return None
