"""miyu_ext 的测试：经假核心走协议，照 Miyu 的协议参考（`docs/reference/extensions.md`）。只用标准库：

    python3 -m unittest discover -s python/tests
"""

import io
import os
import sys
import threading
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from miyu_ext import CoreError, Extension, FakeCore, Reply  # noqa: E402

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


class redirect_stderr:
    """把标准错误收起来：测的是会写标准错误的路子。"""

    def __enter__(self):
        self.saved = sys.stderr
        sys.stderr = io.StringIO()

    def __exit__(self, *_):
        sys.stderr = self.saved


if __name__ == "__main__":
    unittest.main()
