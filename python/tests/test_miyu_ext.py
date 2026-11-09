"""miyu_ext 的测试：经假核心走协议，照 Miyu 的协议参考（`docs/reference/extensions.md`）。只用标准库：

    python3 -m unittest discover -s python/tests
"""

import io
import queue
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from miyu_ext import CoreError, Extension, FakeCore, MethodError, Reply, content_hash  # noqa: E402

SCHEMA = {"type": "object", "properties": {"name": {"type": "string"}}}


def text_of(result):
    return result["blocks"][0]["text"]


class Handshake(unittest.TestCase):
    def test_registers_tools_and_takes_config(self):
        ext = Extension("hello", "0.1.0")

        @ext.tool("hello_greet", "Greet someone.", SCHEMA, access="read", display_name={"en": "Greet"},
                  subject="name", clicall=True)
        def greet(args, call):
            return "hi"

        core = FakeCore(ext, config={"hello.greeting": "Hi"}, language="zh", account="bot")
        hello = core.received[0]
        self.assertEqual(hello["method"], "hello")
        self.assertEqual(hello["params"]["protocol"], [1, 1])
        self.assertEqual(hello["params"]["head"], {"kind": "hello", "version": "0.1.0"})
        self.assertEqual(ext.config, {"hello.greeting": "Hi"})
        self.assertEqual((ext.language, ext.account), ("zh", "bot"))
        self.assertEqual(core.tools, [{
            "name": "hello_greet", "description": "Greet someone.", "input_schema": SCHEMA, "access": "read",
            "venues": ["local"], "display_name": {"en": "Greet"}, "subject": "name", "clicall": True,
        }])
        core.close()
        self.assertFalse(core.extension.is_alive(), "stdin ended, run() returned")

    def test_provides_even_without_tools_and_lists_methods(self):
        ext = Extension("bare")

        @ext.method("status")
        def status(params, caller):
            return {"ok": True}

        core = FakeCore(ext)
        self.assertEqual(core.tools, [], "provide once, empty, so commands and search work")
        self.assertEqual(core.methods, [{"name": "status"}])
        core.close()

    def test_methods_carry_waits_codes_and_can_wait_to_register(self):
        ext = Extension("voice")

        @ext.method("stt.transcribe", timeout_ms=120000, later=True)
        def transcribe(params, caller):
            if "audio" not in params:
                raise MethodError("bad_audio", "No audio.")
            return {"text": "hi"}

        @ext.method("status")
        def status(params, caller):
            return {}

        core = FakeCore(ext)
        self.assertEqual(core.methods, [{"name": "status"}], "later ones wait")
        with self.assertRaises(ValueError):
            ext.enable_methods(["nope"])
        ext.enable_methods(["stt.transcribe"])
        self.assertEqual(core.methods, [{"name": "status"}, {"name": "stt.transcribe", "timeout_ms": 120000}])
        refused = core.request("method.call", {"method": "stt.transcribe", "params": {}})
        self.assertEqual(refused["error"]["code"], -32000)
        self.assertEqual(refused["error"]["data"], {"code": "bad_audio"})
        ext.disable_methods(["stt.transcribe"])
        self.assertEqual(core.methods, [{"name": "status"}])
        core.close()

    def test_a_config_handler_can_call_the_core(self):
        # A handler that calls the core and waits (re-registering methods after a config change) must not run on the
        # reading thread, or the answer is never read (found by miyu-voice, 2026-11-10).
        ext = Extension("cfg")
        got = queue.Queue()

        @ext.method("status")
        def status(params, caller):
            return {"ok": True}

        @ext.on_config
        def changed(keys):
            got.put((keys, ext.call("ping", timeout=5.0)))

        core = FakeCore(ext, answers={"ping": lambda params: {"pong": True}})
        core.notify("extension.config", {"keys": {"cfg.voice": "a"}})
        keys, answer = got.get(timeout=5.0)
        self.assertEqual(keys, {"cfg.voice": "a"})
        self.assertEqual(answer, {"pong": True})
        self.assertEqual(core.request("method.call", {"method": "status", "params": {}}), {"ok": True})
        core.close()

    def test_request_ids_differ_between_starts(self):
        first, second = Extension("a"), Extension("a")
        self.assertTrue(first._id_prefix.startswith("x") and first._id_prefix.endswith("-"))
        self.assertNotEqual(first._id_prefix, second._id_prefix, "a restarted extension must not reuse ids")

    def test_a_protocol_mismatch_exits_with_one(self):
        for reply in ('{"jsonrpc":"2.0","id":"hello","error":{"code":-32010,"message":"no","data":{"reason":"protocol_mismatch"}}}',
                      '{"jsonrpc":"2.0","id":"hello","result":{"protocol":2}}'):
            ext = Extension("old")
            with self.assertRaises(SystemExit) as stopped:
                with redirect_stderr():
                    ext.run(io.StringIO(reply + "\n"), io.StringIO())
            self.assertEqual(stopped.exception.code, 1, reply)


class Tools(unittest.TestCase):
    def setUp(self):
        self.ext = Extension("t")
        ext = self.ext
        self.release = threading.Event()

        @ext.tool("plain", "Text.", access="read")
        def plain(args, call):
            return f"got {args.get('n')}"

        @ext.tool("full", "Full.", access="read")
        def full(args, call):
            return Reply(text="found 2", data={"hits": [1, 2]}, detail={"kind": "links", "links": []})

        @ext.tool("broken", "Breaks.", access="read")
        def broken(args, call):
            raise ValueError("bad input")

        @ext.tool("slow", "Waits.", access="read")
        def slow(args, call):
            self.release.wait(5)
            return "slow done"

        @ext.tool("patient", "Stops when cancelled.", access="read")
        def patient(args, call):
            while not call.cancelled:
                self.release.wait(0.01)
            return "stopped"

        @ext.tool("asks", "Calls the core.", access="read")
        def asks(args, call):
            put = ext.call("blob.put", {"data": "aGk="})
            try:
                ext.call("secret.set", {})
            except CoreError as error:
                return f"{put['blob']} {error.reason}"
            return "not refused"

        self.core = FakeCore(ext, answers={"blob.put": lambda params: {"blob": "sha256:x"}})

    def tearDown(self):
        self.release.set()
        self.core.close()

    def test_a_string_is_one_text_block(self):
        self.assertEqual(self.core.call_tool("plain", {"n": 1}), {"blocks": [{"type": "text", "text": "got 1"}]})

    def test_a_result_carries_data_and_detail(self):
        self.assertEqual(self.core.call_tool("full"), {
            "blocks": [{"type": "text", "text": "found 2"}], "data": {"hits": [1, 2]},
            "detail": {"kind": "links", "links": []},
        })

    def test_a_crash_is_a_failed_result_not_a_dead_extension(self):
        with redirect_stderr():
            failed = self.core.call_tool("broken")
        self.assertEqual(failed["error"], True)
        self.assertIn("bad input", text_of(failed))
        self.assertEqual(text_of(self.core.call_tool("plain", {"n": 2})), "got 2", "still serving")

    def test_calls_run_side_by_side_and_answer_by_id(self):
        slow = {}
        waiting = threading.Thread(target=lambda: slow.update(self.core.call_tool("slow", call_id="c-slow")))
        waiting.start()
        self.assertEqual(text_of(self.core.call_tool("plain", {"n": 3})), "got 3", "the slow one doesn't block it")
        self.release.set()
        waiting.join(5)
        self.assertEqual(text_of(slow), "slow done")

    def test_cancel_reaches_the_call(self):
        answer = {}
        waiting = threading.Thread(target=lambda: answer.update(self.core.call_tool("patient", call_id="c-p")))
        waiting.start()
        self.core.notify("tool.cancel", {"session": "s1", "call_id": "c-p"})
        waiting.join(5)
        self.assertEqual(text_of(answer), "stopped")

    def test_calling_the_core_from_a_tool(self):
        self.assertEqual(text_of(self.core.call_tool("asks")), "sha256:x unknown_method")

    def test_an_unknown_tool_is_method_not_found(self):
        self.assertEqual(self.core.call_tool("ghost")["error"]["code"], -32601)


class Others(unittest.TestCase):
    def test_commands_methods_handlers_and_config(self):
        ext = Extension("o")
        changes = []

        @ext.command("weather")
        def weather(params):
            return f"sunny {params['argument']}"

        @ext.method("status")
        def status(params, caller):
            return {"caller": caller, "params": params}

        @ext.on("user.append")
        def inject(params):
            return {"content": [{"kind": "hint", "text": params.get("text", "")}]}

        ext.on_config(changes.append)
        core = FakeCore(ext, config={"o.a": 1, "o.b": 2})
        said = core.request("command.call", {"command": "weather", "argument": "today", "text": "/weather today"})
        self.assertEqual(said, {"said": "sunny today"})
        self.assertEqual(core.request("method.call", {"method": "status", "params": {"x": 1}, "from": "im"}),
                         {"caller": "im", "params": {"x": 1}})
        self.assertEqual(core.request("method.call", {"method": "nope"})["error"]["code"], -32601)
        self.assertEqual(core.request("user.append", {"session": "s1", "venue": "local", "text": "hi"}),
                         {"content": [{"kind": "hint", "text": "hi"}]})
        self.assertEqual(core.request("search.query", {"query": "x"})["error"]["code"], -32601)
        core.notify("extension.config", {"keys": {"o.a": 5, "o.b": None}})
        core.request("method.call", {"method": "status"})  # 推送照先后到：这一条回来时上一条已经办了
        self.assertEqual(ext.config, {"o.a": 5})
        self.assertEqual(changes, [{"o.a": 5, "o.b": None}])
        core.close()


EDIT_SCHEMA = {"type": "object", "properties": {
    "file_path": {"type": "string"}, "files": {"type": "array", "items": {"type": "string"}},
    "run_in_background": {"type": "boolean"}, "command": {"type": "string"}}, "required": ["file_path"]}


def write(path, text):
    with open(path, "w", encoding="utf-8", newline="") as file:
        file.write(text)


class Declarations(unittest.TestCase):
    """规格多的三格（设计 63 第二节）：开发包照写进 `provide`，假核心照核心查。"""

    def test_the_three_settings_go_into_the_spec(self):
        ext = Extension("d")

        @ext.tool("my_edit", "Edit.", EDIT_SCHEMA, access="write", paths=[{"parameter": "file_path", "write": True}],
                  background_parameters=["run_in_background"], runs_commands=True)
        def edit(args, call):
            return "ok"

        @ext.tool("plain", "Plain.", access="read")
        def plain(args, call):
            return "ok"

        core = FakeCore(ext)
        self.assertEqual(core.tools[0]["paths"], [{"parameter": "file_path", "write": True}])
        self.assertEqual(core.tools[0]["background_parameters"], ["run_in_background"])
        self.assertIs(core.tools[0]["runs_commands"], True)
        self.assertEqual(set(core.tools[1]), {"name", "description", "input_schema", "access", "venues"},
                         "a tool without them is as before")
        self.assertIsNone(core.refused)
        core.close()

    def refused(self, approved=None, **settings):
        ext = Extension("d")
        settings.setdefault("access", "write")

        @ext.tool("my_edit", "Edit.", EDIT_SCHEMA, **settings)
        def edit(args, call):
            return "ok"

        with redirect_stderr():
            core = FakeCore(ext, approved=approved)
            core.close()
        return core.refused

    def test_wrong_declarations_are_bad_tool(self):
        for problem, settings in (
                ("paths", {"paths": [{"parameter": "nowhere"}]}),
                ("paths", {"paths": [{"parameter": "run_in_background"}]}),
                ("paths", {"paths": [{"parameter": "file_path", "write": True}], "access": "read"}),
                ("paths", {"paths": [{"parameter": "file_path", "mode": "w"}]}),
                ("paths", {"paths": [{"parameter": "file_path"}, {"parameter": "file_path"}]}),
                ("paths", {"paths": {"parameter": "file_path"}}),
                ("background_parameters", {"background_parameters": ["file_path"]}),
                ("background_parameters", {"background_parameters": ["nowhere"]}),
                ("runs_commands", {"runs_commands": "yes"})):
            refused = self.refused(**settings)
            self.assertEqual(refused["data"], {"reason": "bad_tool", "tool": "my_edit", "problem": problem}, settings)
        self.assertIsNone(self.refused(paths=[{"parameter": "files"}], access="read"), "an array of strings is fine")

    def test_running_commands_needs_tools_commands_approved(self):
        refused = self.refused(approved=[], runs_commands=True)
        self.assertEqual(refused["data"], {"reason": "needs_approval", "capabilities": ["tools.commands"]})
        self.assertIsNone(self.refused(approved=["tools.commands"], runs_commands=True))
        self.assertIsNone(self.refused(approved=[], runs_commands=False))


class Effects(unittest.TestCase):
    """回应的 `effects`（设计 63 第三、四节）：只收 `checked` 里的、改删要判了写的、四种、写法对的。"""

    def setUp(self):
        self.folder = os.path.realpath(tempfile.mkdtemp())
        self.home = os.path.join(self.folder, "home")
        os.mkdir(self.home)
        self.ext = Extension("e")
        self.seen = []
        self.next = {}

        def answer(args, call):
            self.seen.append(call)
            for path, text in self.next.pop("write", {}).items():
                write(os.path.join(self.folder, path), text)
            for path in self.next.pop("remove", []):
                os.remove(os.path.join(self.folder, path))
            if "raw" in self.next:
                return self.next.pop("raw")
            return Reply(text="done", error=self.next.pop("error", False), effects=self.next.pop("effects", []))

        self.ext.tool("my_edit", "Edit.", EDIT_SCHEMA, access="write",
                      paths=[{"parameter": "file_path", "write": True}, {"parameter": "files"}])(answer)
        self.ext.tool("my_read", "Read.", EDIT_SCHEMA, access="read", paths=[{"parameter": "file_path"}])(answer)
        self.ext.tool("no_paths", "None.", EDIT_SCHEMA, access="write")(answer)
        self.core = FakeCore(self.ext, cwd=self.folder, home=self.home)

    def tearDown(self):
        self.core.close()
        shutil.rmtree(self.folder)

    def real(self, name):
        return os.path.join(self.folder, name)

    def call(self, tool="my_edit", args=None, **next_):
        self.next = next_
        self.core.effects.clear()
        self.core.dropped.clear()
        return self.core.call_tool(tool, args or {"file_path": "a.txt"})

    def whys(self):
        return [(one["kind"], one["why"]) for one in self.core.dropped]

    def test_checked_follows_the_declared_paths(self):
        os.mkdir(self.real("sub"))
        self.call(args={"file_path": "~/n.txt", "files": ["sub/../b.txt", self.real("c.txt"), 3]})
        self.assertEqual(self.seen[-1].checked, [os.path.join(self.home, "n.txt"), self.real("b.txt"),
                                                 self.real("c.txt")], "in order; ~ is home; non-strings left out")
        self.call(tool="no_paths")
        self.assertNotIn("checked", self.seen[-1].params, "no paths declared: no checked")
        self.assertEqual(self.seen[-1].checked, [])
        self.assertIsNone(self.seen[-1].channel, "doesn't run commands: no channel")

    def test_checked_follows_links(self):
        os.mkdir(self.real("there"))
        try:
            os.symlink(self.real("there"), self.real("link"), target_is_directory=True)
        except (OSError, NotImplementedError):
            self.skipTest("no symlinks here")
        self.call(args={"file_path": "link/a.txt"})
        self.assertEqual(self.seen[-1].checked, [self.real(os.path.join("there", "a.txt"))])

    def test_a_change_inside_checked_is_kept(self):
        write(self.real("a.txt"), "old")
        self.call(write={"a.txt": "new"}, effects=Reply().changed("a.txt", before=content_hash(b"old"),
                                                                    after_text="new").effects)
        self.assertEqual(self.core.effects, [{"kind": "file.changed", "path": self.real("a.txt")}])
        self.assertEqual(self.core.dropped, [])
        self.call(write={"n.txt": "made"}, args={"file_path": "n.txt"}, effects=Reply().changed("n.txt").effects)
        self.assertEqual(self.core.effects, [{"kind": "file.changed", "path": self.real("n.txt")}], "a new file")

    def test_paths_outside_checked_are_not_checked(self):
        write(self.real("a.txt"), "a")
        write(self.real("other.txt"), "o")
        self.call(write={"other.txt": "changed"}, effects=Reply().changed("other.txt").read("other.txt").effects)
        self.assertEqual(self.whys(), [("file.changed", "not_checked"), ("file.read", "not_checked")])
        self.assertEqual(self.core.effects, [])
        self.call(tool="no_paths", write={"a.txt": "b"}, effects=Reply().changed("a.txt").effects)
        self.assertEqual(self.whys(), [("file.changed", "not_checked")], "no paths declared: no file effects")

    def test_changing_and_trashing_need_a_write_path(self):
        write(self.real("a.txt"), "a")
        write(self.real("r.txt"), "r")
        self.call(args={"file_path": "a.txt", "files": ["r.txt"]}, write={"r.txt": "changed"}, remove=["a.txt"],
                  effects=Reply().changed("r.txt").read("r.txt", (1, 1)).trashed("a.txt", "/trash/a.txt").effects)
        self.assertEqual(self.whys(), [("file.changed", "not_checked")], "files is declared for reading only")
        self.assertEqual(self.core.effects, [
            {"kind": "file.read", "path": self.real("r.txt"), "lines": [1, 1]},
            {"kind": "file.trashed", "path": self.real("a.txt"), "trash": "/trash/a.txt"}])
        write(self.real("a.txt"), "a")
        self.call(tool="my_read", write={"a.txt": "b"}, effects=Reply().changed("a.txt").trashed("a.txt", "t").effects)
        self.assertEqual(self.whys(), [("file.changed", "not_checked"), ("file.trashed", "not_checked")])

    def test_only_four_kinds(self):
        self.call(effects=[{"kind": "job.started", "job": "x"}, {"kind": "file.shown", "path": "a.txt"},
                           {"kind": "package.state"}])
        self.assertEqual(self.whys(), [("job.started", "kind"), ("file.shown", "kind"), ("package.state", "kind")])
        self.call(effects=Reply().todos([{"content": "Write tests", "status": "in_progress"}]).effects)
        self.assertEqual(self.core.effects, [{"kind": "todo.written",
                                              "todos": [{"content": "Write tests", "status": "in_progress"}]}])

    def test_wrong_shapes_are_dropped(self):
        write(self.real("a.txt"), "a")
        self.call(write={"a.txt": "b"}, effects=[
            "file.changed", {"path": "a.txt"}, {"kind": "file.changed"}, {"kind": "file.changed", "path": 1},
            {"kind": "file.changed", "path": "a.txt", "before": "md5:x"},
            {"kind": "file.read", "path": "a.txt", "lines": [0, 3]},
            {"kind": "file.read", "path": "a.txt", "lines": [3, 2]},
            {"kind": "file.trashed", "path": "a.txt", "trash": "x" * 4097},
            {"kind": "todo.written", "todos": [{"content": "x"}]},
            {"kind": "todo.written", "todos": [], "done": None},
            {"kind": "file.changed", "path": "a.txt"}, {"kind": "file.changed", "path": "a.txt"}])
        self.assertEqual(self.whys(), [("-", "shape"), ("-", "shape")] + [("file.changed", "shape")] * 3
                         + [("file.read", "shape")] * 2 + [("file.trashed", "shape")] + [("todo.written", "shape")] * 2
                         + [("file.changed", "shape")], "the second report of the same file too")
        self.assertEqual(len(self.core.effects), 1)
        self.next = {"raw": {"blocks": [], "effects": {"kind": "file.changed"}}}
        self.core.dropped.clear()
        self.core.call_tool("my_edit", {"file_path": "a.txt"})
        self.assertEqual(self.whys(), [("-", "shape")], "effects that aren't a list are dropped whole")
        self.next = {"raw": {"blocks": [], "effects": None}}
        self.core.dropped.clear()
        self.core.call_tool("my_edit", {"file_path": "a.txt"})
        self.assertEqual(self.whys(), [], "null is not written")

    def test_the_core_reads_the_file_itself(self):
        write(self.real("a.txt"), "same")
        self.call(write={"a.txt": "same"}, effects=Reply().changed("a.txt").effects)
        self.assertEqual(self.whys(), [("file.changed", "unchanged")])
        self.call(write={"a.txt": "new"}, effects=Reply().changed("a.txt", before_text="not it").effects)
        self.assertEqual(self.whys(), [("file.changed", "mismatch")])
        self.call(write={"a.txt": "newer"}, effects=Reply().changed("a.txt", after=content_hash(b"x")).effects)
        self.assertEqual(self.whys(), [("file.changed", "mismatch")])
        self.call(args={"file_path": "n.txt"}, write={"n.txt": "made"},
                  effects=Reply().changed("n.txt", before_text="").effects)
        self.assertEqual(self.whys(), [("file.changed", "mismatch")], "a new file had no before")
        self.call(effects=Reply().trashed("a.txt", "t").effects)
        self.assertEqual(self.whys(), [("file.trashed", "mismatch")], "still there")
        self.call(args={"file_path": "gone.txt"}, effects=Reply().changed("gone.txt").effects)
        self.assertEqual(self.whys(), [("file.changed", "mismatch")], "not there after")

    def test_too_big(self):
        with open(self.real("big"), "wb") as file:
            file.truncate(20 * 1024 * 1024 + 1)
        self.call(args={"file_path": "big"}, effects=Reply().changed("big").read("big").effects)
        self.assertEqual(self.whys(), [("file.changed", "too_big"), ("file.read", "too_big")])

    def test_an_error_keeps_files_but_not_todos(self):
        write(self.real("a.txt"), "a")
        self.call(error=True, write={"a.txt": "half"},
                  effects=Reply().changed("a.txt").todos([{"content": "x", "status": "pending"}]).effects)
        self.assertEqual(self.core.effects, [{"kind": "file.changed", "path": self.real("a.txt")}])
        self.assertEqual(self.core.dropped, [], "the to-do list is left out without a word")


class Commands(unittest.TestCase):
    """`runs_commands` 的通道（设计 63 第六节第 2 条）：`tool.call` 带 `channel`，`command_env` 设进子进程。"""

    def test_the_channel_reaches_the_command(self):
        ext = Extension("c")
        script = ("import json, os; print(json.dumps([os.environ.get('MIYU_CALL_SOCK'), "
                  "os.environ.get('MIYU_CALL_TOKEN'), os.environ['PATH'].split(os.pathsep)[0]]))")

        @ext.tool("my_shell", "Run.", EDIT_SCHEMA, access="execute", runs_commands=True)
        def run(args, call):
            done = subprocess.run([sys.executable, "-c", script], env=call.command_env(), capture_output=True,
                                  text=True, check=True)
            return done.stdout.strip()

        core = FakeCore(ext, approved=["tools.commands"])
        sock, token, first = json.loads(core.call_tool("my_shell", {"file_path": "x"})["blocks"][0]["text"])
        self.assertEqual((sock, token, first), (core.channel["sock"], core.channel["token"], core.channel["bin"]))
        core.channel = None
        sock, token, first = json.loads(core.call_tool("my_shell", {"file_path": "x"})["blocks"][0]["text"])
        self.assertEqual((sock, token), (None, None), "no channel: nothing set")
        core.close()

    def test_command_env_without_bin_keeps_path(self):
        from miyu_ext import Call
        call = Call({"channel": {"sock": "/s", "token": "t" * 32}})
        env = call.command_env({"PATH": "/usr/bin", "KEEP": "1"})
        self.assertEqual(env, {"PATH": "/usr/bin", "KEEP": "1", "MIYU_CALL_SOCK": "/s", "MIYU_CALL_TOKEN": "t" * 32})
        call = Call({"channel": {"sock": "/s", "token": "t", "bin": "/b"}})
        self.assertEqual(call.command_env({})["PATH"], "/b", "no PATH before: just bin")
        self.assertEqual(Call({}).command_env({"A": "1"}), {"A": "1"})


class redirect_stderr:
    """把标准错误收起来：测的是会写标准错误的路子。"""

    def __enter__(self):
        self.saved = sys.stderr
        sys.stderr = io.StringIO()

    def __exit__(self, *_):
        sys.stderr = self.saved


if __name__ == "__main__":
    unittest.main()
