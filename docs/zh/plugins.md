# 插件（编译期静态）

[English](../plugins.md) | 简体中文

codest 的插件是一个编译进二进制的普通 Rust 值，它扩展 agent 的能力，但不碰
core、不碰终端：`/help`、`/clear`、`/quit`、`/status`、`/continue` 以及
`bash`/`fs` 工具集全部是插件——不存在特权命令层。本文覆盖插件契约；第三方
插件的安装见 [local-plugins.md](../local-plugins.md)。

## 1. 总览

一个插件可以通过 [`Plugin`](../../crates/cst-plugin-api/src/plugin.rs) trait
贡献以下任意子集：

| 扩展点 | trait 方法 | 典型用途 |
| --- | --- | --- |
| 斜杠命令 | `commands()` + `run_command()` | `/help`、`/clear`、`/status` |
| 系统提示词片段 | `system_prompt()` | 领域指令、工具使用约定 |
| 生命周期钩子 | `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` | 通知、报告、请求宿主动作 |
| 工具 | `tools()` + `run_tool()` | 模型可调用的能力（`bash`、`read`…） |
| 协议工厂 | `protocols()` | 给 `providers.toml` 实例接新线协议 |
| 登录入口 | `auth_providers()` / `auth_status()` / `begin_sign_in()` | `/provider` 后面的浏览器/PAT 登录 |

插件是**编译期静态**的：插件是一个带 `static` 值的 `struct`，注册在一个
`static` 切片里。运行时没有任何发现或加载——注册表就是一个可读的数组。收益
是确定性的注册顺序（= 钩子顺序 = 菜单顺序）、装配期整体校验、以及全程纯
Rust 类型；代价是新增插件必须重新编译（取舍见第 6 节；
[local-plugins.md](../local-plugins.md) 覆盖第三方插件的接入）。

## 2. 插件契约

trait 及其交换的数据都在契约 crate
[`cst-plugin-api`](../../crates/cst-plugin-api/README.md) 里（
`PLUGIN_API_VERSION` 当前为 `1`），因此外部插件实现契约不需要依赖宿主
二进制。宿主侧在 [`src/plugins/mod.rs`](../../src/plugins/mod.rs)。

### trait 方法

| 方法 | 接收 | 返回 | 默认 |
| --- | --- | --- | --- |
| `id()` | — | `&'static str` | —（必填） |
| `commands()` | — | `&'static [CommandSpec]` | 空 |
| `system_prompt()` | — | `Option<&'static str>` | `None` |
| `tools()` | — | `&'static [ToolSpec]` | 空 |
| `run_tool(&ToolCall)` | 原始 JSON 参数 | `Result<String, PluginError>` | 报错 |
| `protocols()` | — | `&'static [&'static dyn ProtocolFactory]` | 空 |
| `auth_providers()` | — | `&'static [AuthProvider]` | 空 |
| `auth_status(&str)` | 实例名 | `AuthStatus` | `Unsupported` |
| `begin_sign_in(...)` | 实例、方法、secret、notifier、取消 token | `Option<SignInFuture>` | `None` |
| `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` | `&mut PluginContext` | — | 无操作 |
| `run_command(&Invocation, &mut PluginContext)` | 解析后的命令 + 参数 | `Result<(), PluginError>` | 报错 |

插件以 `&'static dyn Plugin` 共享，所以全部方法只拿 `&self`；有状态的插件用
内部可变性（`Mutex`、原子类型）。命令规格、提示词片段、工具规格都是
`'static` 字符串，菜单和帮助文本直接借用，不做克隆。

### 副作用，而非终端调用

插件从不接触终端、会话或 ratatui——契约 crate 里没有任何终端代码。插件只在
`PluginContext` 上**描述**想发生的事（请求副作用），由前端收集并逐条应用。
这是插件影响世界的唯一途径：

| 效果 | 请求方式 | 前端行为 |
| --- | --- | --- |
| `Notice(String)` | `ctx.notice(text)` | 向 transcript 追加通知 cell |
| `Error(String)` | `ctx.error(text)` | 追加错误 cell |
| `Report(ReportBlock)` | `ctx.report(block)` | 追加带标题的报告块（`/context` 形态） |
| `ClearConversation` | `ctx.clear_conversation()` | 清空 transcript 与 core 会话 |
| `Quit` | `ctx.quit()` | 下一帧退出 |
| `SetComposer(String)` | `ctx.set_composer(text)` | 替换输入框内容 |
| `SubmitUserTurn(String)` | `ctx.submit_user_turn(text)` | 提交一轮用户消息——仅空闲时生效 |
| `ContinueTurn` | `ctx.continue_turn()` | 不追加新消息、直接驱动当前 transcript（`/continue`）——仅空闲时生效 |

副作用按插件顺序收集、按同一顺序应用。受空闲门控的效果让递归不可能：
`on_user_message` 在 `busy` 置位之后运行，`on_turn_complete` 在其清除之前
运行，所以钩子里请求 `SubmitUserTurn` 是无操作而不是循环。命令处理时 agent
是空闲的，所以"提交一段 prompt"的命令可以工作。

### 钩子能看到什么

`PluginContext` 暴露只读的会话事实加上上述副作用请求：`command_table()`、
`model()`、`mode()`、`transcript_len()`、`message_count()`、
`context_window()`、`last_usage()`、`total_input()`、`total_output()`、
`plugin_ids()`、`system_sections()`、`effort()`、`request()`（protocol、
base URL、endpoint、`body_overrides`）、`tools()`、`enabled_tools()`——
从不暴露 agent 会话或事件循环。错误是软性的：`run_command` / `run_tool`
返回 `Err` 会变成 `Error` 效果（命令），或把 `error: …` 回传给模型并在
transcript 显示一行失败（工具）；注册表层面的问题是启动错误（第 4 节）；
插件 panic 不会被捕获——静态插件是可信代码，与二进制其他模块同级。

## 3. 内置插件

注册表在 [`src/plugins/builtin/mod.rs`](../../src/plugins/builtin/mod.rs)：

```rust
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    &help::HELP, &clear::CLEAR, &quit::QUIT, &status::STATUS,
    &r#continue::CONTINUE, &tools::BASH, &tools::FS,
];
```

顺序就是契约：它是钩子执行顺序、命令菜单顺序，也是系统提示词的拼接顺序。
工具插件排最后，让它们的提示词段落落在命令插件的贡献之后。

| 插件（id） | 贡献 | 行为 |
| --- | --- | --- |
| `help` | `/help`（`/h`、`/?`） | 渲染共享命令表 |
| `clear` | `/clear` | 请求 `ClearConversation` |
| `quit` | `/quit`（`/exit`） | 请求 `Quit` |
| `status` | `/status` | backend、模式、transcript 大小、上下文用量、token 合计、插件列表 |
| `continue` | `/continue` | 请求 `ContinueTurn`——恢复被打断的回合 |
| `bash` | `bash` 工具 + 提示词段落 | 执行一条 shell 命令 |
| `fs` | `read`、`write`、`edit` 工具 | 文件操作 |

拆成两个工具插件而不是一个，是为了让 profile 的工具过滤和 `/tools` 开关按
插件生效；更细的粒度由每个 `ToolSpec` 的 `read_only` 标记承担。

**`bash`**（[tools/bash.rs](../../src/plugins/builtin/tools/bash.rs)）在工作
目录里 spawn `$SHELL -c <command>`（回退 `/bin/sh`）——一次性命令，不是持久
shell。`timeout` 参数默认 300s、上限 600s；超时杀掉整个进程组（子进程在
spawn 时被放进独立进程组），所以后台子进程不会比调用本身活得更久。两条管道
由线程排空以防死锁，每条流最多收集 1 MiB，回传给模型前做尾部截断。

**`fs`**（[tools/fs.rs](../../src/plugins/builtin/tools/fs.rs)）实现
`read`（带行号的窗口，`offset`/`limit` 续读，拒绝二进制文件，隐藏 BOM）、
`write`（创建或整文件覆盖，自动建父目录）、`edit`（精确
`old_string`/`new_string` 替换——匹配必须唯一，除非传 `replace_all`）。
`read` 标记 `read_only`，两个改写工具没有；该标记驱动 `/tools` 标记与只读
profile。

输出上限由 [tools/truncate.rs](../../src/plugins/builtin/tools/truncate.rs)
统一承担：每个结果 **2000 行 / 50 KB**，单行超过 2000 字符截断。shell 输出
保留**尾部**（长构建之后最近的输出才重要）；文件读取保留**头部**并报告下一个
`offset`。

工具分派的完整链路：请求带上声明的工具，模型流式返回 `tool_calls`，worker
按名字把每次调用派发给声明它的插件（`PluginHost` 实现了 core 的
`ToolExecutor` trait），结果以 `role: "tool"` 消息回传。默认不设轮数上限——
模型不再调工具、用户中断或请求失败时结束；`config.toml` 的
`max_tool_rounds` 可恢复兜底。工具执行跑在阻塞线程池上，所以 `run_tool`
可以阻塞；钩子不行。

## 4. 装配期校验

`PluginHost::new` / `with_local` 在任何东西运行之前校验整个注册表，失败是
**启动错误**而不是运行时警告：

| 校验 | 错误 |
| --- | --- |
| 插件 id 非空且唯一 | `EmptyPluginId` / `DuplicatePluginId` |
| 命令名/别名合法且全局唯一 | `CommandTable(InvalidName)` / `(DuplicateName)` |
| 工具名跨插件唯一 | `DuplicateTool` |
| profile 工具过滤引用已知插件 | `UnknownFilteredPlugin` |

命令名允许小写 ASCII 字母、数字、`-`、`_`、`?`（最后一个是 `/help` 的
`/?` 别名）。内置组与本地组之间统一校验重复，所以本地插件永远不能遮蔽内置
插件。

命令表还携带**前端自带命令**——`/new`、`/rename`、`/sessions`、`/model`、
`/provider`、`/effort`、`/thinking`、`/profile`、`/tools`、`/context`。
它们不属于任何插件；放进同一张表是为了让解析、菜单和 `/help` 保持一致，前端
在分派给插件之前先按命令名拦截处理。`PluginHost::shipped()` 只用内置插件
构造 host——一张测试与诊断用的稳定表，不随用户装了什么插件而变化。

## 5. 新增第一个插件

一个最小的命令插件，取材自
[help.rs](../../src/plugins/builtin/help.rs) 与
[clear.rs](../../src/plugins/builtin/clear.rs)：

```rust
// src/plugins/builtin/review.rs
use crate::plugins::{ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError};

pub static REVIEW: Review = Review;

pub struct Review;

static COMMANDS: [CommandSpec; 1] = [CommandSpec::new(
    "review",
    &[],
    "/review <focus>",
    "ask the model to review the current change",
    ArgsPolicy::Required,
)];

impl Plugin for Review {
    fn id(&self) -> &'static str {
        "review"
    }
    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }
    fn run_command(
        &self,
        invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        ctx.submit_user_turn(format!(
            "Review the current change. Focus: {}",
            invocation.args
        ));
        Ok(())
    }
}
```

注册它——模块之外唯一要改的文件是注册表
`src/plugins/builtin/mod.rs`：

```rust
mod review;
pub static BUILTIN_PLUGINS: &[&dyn Plugin] = &[
    // ... 既有条目 ...
    &review::REVIEW,
];
```

改动到此为止：core、TUI、CLI 都不用动——解析、别名匹配、参数策略
（`Required` 会拒绝裸 `/review`）、补全、菜单和 `/help` 都会从共享的
`CommandTable` 自动获得新命令。然后：

```bash
cargo test          # 离线；补一个断言副作用序列的 host 层测试
cargo clippy --all-targets -- -D warnings
```

纯钩子插件可以完全不写 `commands()`，只实现比如 `on_turn_complete`；工具
插件实现 `tools()` + `run_tool`，规格为
`ToolSpec { name, description, parameters_json, read_only }`——schema 是
原始 JSON-Schema 文本，这样它能放进 `const`。状态放在 `Mutex` 里，与内置
插件一样走内部可变性。

## 6. 设计笔记

**为什么编译期静态而不是动态加载 `.so`？** 动态加载需要自建 C ABI、版本
协商、panic/FFI 桥接——在当前规模下风险远大于收益。静态注册表给出确定性
（没有发现顺序、没有加载失败、没有运行时版本不匹配）、可读性、接近零的开销。
`inventory`/`linkme` 方式的分布式注册也被否决：链接器 section 行为和平台
相关的注册顺序比显式数组更差；若将来重拾，插件契约也不需要变。

**为什么是副作用？** 因为它们是纯数据，任何前端——TUI、无终端 runner、测试
——都可以按自己的方式应用，这正是插件不依赖 ratatui、整个系统不需要终端就能
测试的原因。

**有意设置的边界：** 钩子在前端线程上同步执行，必须快速返回（慢工作走工具
调用这条正道）；没有沙箱与崩溃隔离——插件以宿主权限运行；状态在进程内，随
进程退出而消失；插件不能画自定义 widget、不能注册键位。外部插件沿用同一份
契约，由生成的 wrapper crate 编译进二进制（
[local-plugins.md](../local-plugins.md)）。

## 7. 相关文档

- [local-plugins.md](../local-plugins.md) —— 安装与管理外部插件
- [multi-provider.md](../multi-provider.md) —— provider 实例、wire quirks、自定义协议
- [profiles.md](../profiles.md) —— profile 预设与 `tools` 过滤
- [../../README.md](../../README.md) —— 上手指南
- 设计笔记（中文）：[provider.md](../provider.md)、[session-id.md](../session-id.md)、[session-continue.md](../session-continue.md)
- 契约 crate 参考：[../../crates/cst-plugin-api/README.md](../../crates/cst-plugin-api/README.md)
