# miyu-sdk 仓库的规矩

给第三方作者写 Miyu 扩展用的开发包：Python 一份（`python/`），Rust 一份（`rust/`）。设计在核心仓库 `miyu-agent/miyu` 的 `docs/designs/40-开发包.md`，协议在它的 `docs/reference/`。照核心仓库的 `AGENTS.md`，下面是和这个仓库有关的几条。

1. **只走协议**：开发包只包作者常用的那几样（握手、收发、并发的调用、取消、配置推送、退出码），不给扩展多出协议以外的本事；协议缺什么补进核心。
2. **一一对应**：两份的概念、名字对得上（`Extension`、`tool`、`command`、`method`、`on`、`call`、`fatal`、`FakeCore`）；改一份，另一份跟着改。
3. **Python 那份只用标准库、一个文件** `python/miyu_ext.py`，支持 Python 3.9 起；作者能直接拷进自己的包。Rust 那份只依赖 `serde`、`serde_json`，同步。
4. **说明的语言**：文档字符串、代码里给作者看的说明写英文（作者在编辑器、`help()` 里看）；README 中英两份，中文是正本。2026-10-20 主会话定：核心仓库「注释用中文」管的是核心的代码，这里的说明是给全世界的作者的。
5. **测试**：每份带一个假核心，测试照它走协议；例子包也要照它跑通。Python：`python3 -m unittest discover -s python/tests`。
6. **许可证**：Apache-2.0（2026-10-26 项目主人定分层：核心 AGPL、开发包和模板 Apache，核心仓库 README「许可证」）。
