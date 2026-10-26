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

import json
import os
import sys
import threading
import traceback

__version__ = "0.1.0"

#: The protocol major versions this file speaks.
PROTOCOL = (1, 1)

#: Exit code for "restarting won't help" (bad config, port taken, protocol mismatch): the core stops instead of retrying.
EXIT_CONFIG = 1

_METHOD_NOT_FOUND = -32601
_INTERNAL_ERROR = -32603


class CoreError(Exception):
    """The core refused a request: `reason` is the stable code (`data.reason`), `message` is for people."""

    def __init__(self, error):
        self.code = error.get("code")
        self.data = error.get("data") or {}
        self.reason = self.data.get("reason")
        self.message = error.get("message", "")
        super().__init__(f"{self.reason or self.code}: {self.message}")


class Reply:
    """A full tool result. Returning a string from a tool is the same as `Reply(text=...)`.

    `blocks` are what the model sees; `error` marks a failure (explain it in the text). `data` goes only to scripts that call
    the tool with `miyu call`; `detail` only to the interfaces (links, table or images shape); `sent` are images she sends;
    `end_turn` ends the turn after this tool.
    """

    def __init__(self, text=None, blocks=None, error=False, data=None, detail=None, sent=None, end_turn=False):
        self.blocks = list(blocks or [])
        if text is not None:
            self.blocks.insert(0, {"type": "text", "text": text})
        self.error = error
        self.data = data
        self.detail = detail
        self.sent = sent
        self.end_turn = end_turn

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
        return out


class Call:
    """One `tool.call`: the arguments (already repaired to the schema), who asked and where. Check `cancelled` in long work:
    it turns true when the call timed out or the person interrupted."""

    def __init__(self, params):
        self.params = params
        self.session = params.get("session")
        self.call_id = params.get("call_id")
        self.tool = params.get("tool")
        self.cwd = params.get("cwd")
        self.by = params.get("by")
        self.owner = params.get("owner", False)
        self._cancelled = threading.Event()

    @property
    def cancelled(self):
        return self._cancelled.is_set()


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
        self._handlers = {}
        self._config_handlers = []
        self._ready_handlers = []
        self._out = None
        self._write_lock = threading.Lock()
        self._waiting = {}
        self._waiting_lock = threading.Lock()
        self._next_id = 0
        self._calls = {}

    # Registering ---------------------------------------------------------------------------------------------------

    def tool(self, name, description, schema=None, *, access, venues=("local",), timeout_ms=None, display_name=None,
             subject=None, need_load=None, clicall=None, feature=None):
        """Register a tool. The function gets `(args, call)` and returns a string, a `Reply` or a raw result dict.

        `description` and the schema are for the model: short English sentences, at most three. `access` is honest:
        `read`, `write`, `execute`, `network`, `outbound` or `venue`.
        """
        spec = {
            "name": name,
            "description": description,
            "input_schema": schema or {"type": "object", "properties": {}},
            "access": access,
            "venues": list(venues),
        }
        for key, value in (("timeout_ms", timeout_ms), ("display_name", display_name), ("subject", subject),
                           ("need_load", need_load), ("clicall", clicall), ("feature", feature)):
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

    def method(self, name):
        """Handle the method `name` called from your page, a widget button or another package (`method.call`). The
        function gets `(params, caller)`: `caller` is the calling package's id, or `None` for a person."""

        def register(function):
            self._methods[name] = function
            return function

        return register

    def on(self, method):
        """Handle any other request or notification from the core, such as `user.append`, `search.query` or
        `widget.watch`. The function gets the params; for a request, what it returns is the answer."""

        def register(function):
            self._handlers[method] = function
            return function

        return register

    def on_config(self, function):
        """Call `function(changed)` after a config push; `changed` maps keys to new values (`None` when unset)."""
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
            request_id = f"x{self._next_id}"
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
                self.call("package.methods", {"methods": [{"name": name} for name in self._methods]})
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
                function(changed)
        elif method == "tool.cancel":
            call = self._calls.get(params.get("call_id"))
            if call is not None:
                call._cancelled.set()
        elif method in self._handlers:
            self._handlers[method](params)

    def _answer(self, request_id, method, params):
        try:
            result = self._handle(method, params)
        except _NotFound:
            self._send({"jsonrpc": "2.0", "id": request_id,
                        "error": {"code": _METHOD_NOT_FOUND, "message": f"no handler for {method}"}})
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
    """

    def __init__(self, ext, config=None, language="en", account="admin", protocol=PROTOCOL[0], answers=None):
        self.received = []
        self.tools = []
        self.methods = []
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
        """Call `tool` the way the core does; returns the result dict (or the JSON-RPC error as `{"error": ...}`)."""
        call_id = call_id or f"c{self._next + 1}"
        return self.request("tool.call", {"session": session, "call_id": call_id, "tool": tool, "args": args or {},
                                          "cwd": "/", "owner": owner}, timeout)

    def request(self, method, params, timeout=10.0):
        """Send any core-to-extension request (`command.call`, `method.call`, `user.append`...) and wait for the answer."""
        with self._lock:
            self._next += 1
            request_id = f"core-{self._next}"
            box = {"event": threading.Event()}
            self._waiting[request_id] = box
        self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        if not box["event"].wait(timeout):
            raise TimeoutError(f"no answer to {method} within {timeout} seconds")
        message = box["message"]
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

    def _provided(self, params):
        self.tools = params.get("tools", [])
        if not self._expects_methods:
            self._registered.set()
        return {"tools": len(self.tools)}

    def _methods_listed(self, params):
        self.methods = params.get("methods", [])
        self._registered.set()
        return {}

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
            reply = {"result": function(message.get("params", {}))}
        self._write({"jsonrpc": "2.0", "id": message["id"], **reply})

    def _write(self, message):
        with self._lock:
            self._writer.write(json.dumps(message, ensure_ascii=False) + "\n")
            self._writer.flush()
