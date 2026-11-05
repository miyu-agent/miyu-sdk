"""例子包 `examples/hello`、`examples/replace` 照假核心跑一遍：作者照它抄，它就得是对的。"""

import os
import runpy
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))

from miyu_ext import FakeCore  # noqa: E402

PROGRAM = os.path.join(HERE, "..", "examples", "hello", "bin", "miyu-hello")
REPLACE = os.path.join(HERE, "..", "examples", "replace", "bin", "miyu-replace")


class Example(unittest.TestCase):
    def test_hello_greets_with_the_setting(self):
        ext = runpy.run_path(PROGRAM, run_name="example")["ext"]
        core = FakeCore(ext, config={"hello.greeting": "Hi"})
        self.assertEqual([tool["name"] for tool in core.tools], ["hello_greet"])
        answer = core.call_tool("hello_greet", {"name": "Ann"})
        self.assertEqual(answer, {"blocks": [{"type": "text", "text": "Hi, Ann!"}]})
        core.notify("extension.config", {"keys": {"hello.greeting": None}})
        answer = core.call_tool("hello_greet", {"name": "Bo"})
        self.assertEqual(answer["blocks"][0]["text"], "Hello, Bo!", "unset falls back to the default")
        core.close()

    def test_replace_edits_the_checked_file_and_reports_it(self):
        ext = runpy.run_path(REPLACE, run_name="example")["ext"]
        with tempfile.TemporaryDirectory() as folder:
            target = os.path.join(folder, "a.txt")
            with open(target, "w", encoding="utf-8") as file:
                file.write("hello world\n")
            core = FakeCore(ext, cwd=folder)
            self.assertEqual(core.tools[0]["paths"], [{"parameter": "file_path", "write": True}])
            answer = core.call_tool("replace_text", {"file_path": "a.txt", "old": "world", "new": "there"})
            self.assertEqual(answer["blocks"][0]["text"], "Replaced in a.txt.")
            with open(target, encoding="utf-8") as file:
                self.assertEqual(file.read(), "hello there\n")
            real = os.path.realpath(target)
            self.assertEqual(core.effects, [{"kind": "file.changed", "path": real}], "kept, like the built-in edit")
            self.assertEqual(core.dropped, [])
            same = core.call_tool("replace_text", {"file_path": "a.txt", "old": "there", "new": "there"})
            self.assertNotIn("error", same)
            self.assertEqual(core.dropped, [{"tool": "replace_text", "kind": "file.changed", "why": "unchanged"}])
            missing = core.call_tool("replace_text", {"file_path": "a.txt", "old": "nowhere", "new": "x"})
            self.assertEqual(missing["error"], True)
            core.close()


if __name__ == "__main__":
    unittest.main()
