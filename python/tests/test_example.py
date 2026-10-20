"""例子包 `examples/hello` 照假核心跑一遍：作者照它抄，它就得是对的。"""

import os
import runpy
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))

from miyu_ext import FakeCore  # noqa: E402

PROGRAM = os.path.join(HERE, "..", "examples", "hello", "bin", "miyu-hello")


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


if __name__ == "__main__":
    unittest.main()
