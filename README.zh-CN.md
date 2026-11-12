# codest

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](https://github.com/iicrom/codest/pulls)

[English](README.md) | 简体中文

**codest**（简称 **`cst`**）是一个从 [OpenAI codex](https://github.com/openai/codex) 的 `tui` + `core` 架构中提炼出来的**最小 TUI chat agent**。它连接任意 OpenAI Chat Completions 兼容端点，覆盖最核心的能力：流式回复、工具调用、会话持久化，以及一套编译期静态插件系统——整个项目是一个刻意保持精简、可以端到端读完的 Rust crate。

codest **不是** codex 的克隆，也刻意不做 codex 的运行管控机制：没有沙箱、没有审批流、没有 MCP、没有图片输入。它保留的是**架构**——`Op`/`Event` 消息总线、会话 worker、流式 SSE 客户端、rollout 持久化与 ratatui TUI——蒸馏为可独立编译的模块。

```bash
cargo install --git https://github.com/iicrom/codest
codest    # 或：cst
```

主要特性：`/model` 多 Provider 切换、命名 profile、带会话级开关的 `bash`/`fs` 工具、UUIDv7 会话恢复、thinking 与工具卡片、带语法高亮的 Markdown 回复，以及 fullscreen / inline 双 TUI 模式。

## 文档

### 快速上手

| 文档 | 内容 | English |
| --- | --- | --- |
| [快速开始](docs/zh/getting-started.md) | 环境要求、安装、快速开始、CLI 参考。 | [EN](docs/getting-started.md) |
| [配置](docs/zh/configuration.md) | `~/.codest` 下的配置文件、`config.toml`、环境变量。 | [EN](docs/configuration.md) |
| [指令与按键](docs/zh/commands.md) | 全部 slash 指令与快捷键。 | [EN](docs/commands.md) |

### 使用

| 文档 | 内容 | English |
| --- | --- | --- |
| [多会话](docs/zh/sessions.md) | 多会话工作流、rollout 持久化、`/continue` 恢复、session 头。 | [EN](docs/sessions.md) |
| [工具调用](docs/zh/tools.md) | 工具链路、内置 `bash`/`fs` 工具、`/tools` 开关、工具卡片。 | [EN](docs/tools.md) |
| [终端界面](docs/zh/tui.md) | fullscreen 与 inline、Markdown 渲染、代码高亮、thinking 卡片。 | [EN](docs/tui.md) |

### 扩展

| 文档 | 内容 | English |
| --- | --- | --- |
| [插件](docs/zh/plugins.md) | 插件契约、内置插件、编写第一个插件。 | [EN](docs/plugins.md) |
| [外部插件](docs/zh/local-plugins.md) | `codest plugin install`、仓库外 wrapper crate、契约版本规则。 | [EN](docs/local-plugins.md) |
| [多 Provider](docs/zh/multi-provider.md) | Provider 实例、wire quirks、模型发现、`/provider`、自定义协议。 | [EN](docs/multi-provider.md) |
| [Profiles](docs/zh/profiles.md) | Profile 预设、选择优先级、装配期与会话期值。 | [EN](docs/profiles.md) |

### 内部实现

| 文档 | 内容 | English |
| --- | --- | --- |
| [架构](docs/zh/architecture.md) | 消息流、从 codex 提取了什么、项目结构、扩展点。 | [EN](docs/architecture.md) |
| [开发](docs/zh/development.md) | 构建、离线测试、冒烟脚本、已知边界、路线图、贡献指南。 | [EN](docs/development.md) |
| [设计笔记](docs/README.md#design-notes-chinese-only) | 更深入的设计理由，仅中文：`/provider` 向导、会话 id、`/continue`。 | — |

## 许可证

基于 [Apache License, Version 2.0](LICENSE) 开源。
