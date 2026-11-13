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
| 事件流 | `subscriptions()` + `on_event()` | 计量器、监控端、对 agent 运行的响应 |
| 状态栏分段 | `ctx.set_status()` / `ctx.clear_status()` | 页脚读数（`tps:rate`、时钟、倒计时） |
| 后台任务 | `ctx.spawn(future)` + `ctx.bridge()` | 长驻工作：监控服务器、刷新循环 |

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
| `subscriptions()` | — | `EventSubscription` | `NONE` |
| `on_event(&PluginEvent, &mut PluginContext)` | 单个核心事件 + 上下文 | — | 无操作 |
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
| `SetStatus(StatusSegment)` | `ctx.set_status(segment)` | 按键 upsert 一个页脚分段（`plugin:key`） |
| `ClearStatus(key)` | `ctx.clear_status(key)` | 移除一个页脚分段 |

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
    &r#continue::CONTINUE, &tps::TPS, &tools::BASH, &tools::FS,
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
| `tps` | `/tps` | 页脚的实时 tokens-per-second 计量（默认关闭） |
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

## 6. 观测 agent：事件流、状态栏分段、后台任务

四个生命周期钩子覆盖了回合边界。响应式插件需要的其余能力由三个原语承担，
而且每一个都是有意做成通用形态：它们的存在是为了撑起一**族**插件，而不是
每来一个想法就给宿主加一条 API。

### 事件流

声明想要的类别，然后实现一个钩子：

```rust
use crate::plugins::{
    DeltaKind, EventSubscription, Plugin, PluginContext, PluginEvent, PluginEventKind,
};

impl Plugin for MyMeter {
    fn id(&self) -> &'static str {
        "my-meter"
    }

    fn subscriptions(&self) -> EventSubscription {
        EventSubscription::DELTAS
            .union(EventSubscription::USAGE)
            .union(EventSubscription::LIFECYCLE)
    }

    fn on_event(&self, event: &PluginEvent<'_>, ctx: &mut PluginContext<'_>) {
        match event.kind {
            // `event.at` 是宿主的单调发射时间戳。
            PluginEventKind::MessageDelta { kind: DeltaKind::Text, text, .. } => {
                let _ = (text, ctx);
            }
            // 后端上报的真实 token 数。
            PluginEventKind::TurnUsage { output, .. } => {
                let _ = output;
            }
            _ => {}
        }
    }
}
```

类别：`LIFECYCLE`（回合开始/落定/中止/结束、错误、清空、关闭）、
`DELTAS`（正文与思考的流式增量）、`USAGE`（每轮与每回合的 token 数）、
`TOOLS`（工具调用生命周期，含耗时）、`SESSION`（会话/模型/effort/profile/
工具集切换）、`SNAPSHOT`（完整会话快照）。未订阅的类别零开销：宿主在派发
前就完成过滤，只要回合边界的插件永远不会收到一份流式增量的拷贝。

两个事实让这条流"可计量"：

* **`event.at` 是宿主的单调发射时间戳**，在 `Event::new` 里统一打点——
  吞吐计量器据此取时钟，而不是靠到达顺序去猜。
* **`SNAPSHOT` 事件携带完整会话**——宿主在插件启动时和每次会话切换后都会
  向 worker 索取一份，监控端可以从中途接入而无需重放历史。"拿不准就重新
  快照"就是预期的恢复策略，与参考监控端一致。

载荷是从事件借用来的；插件想留存增量就自己拷贝。四个生命周期钩子原样
保留——`on_event` 是它们的超集，内置的 `tps` 插件
（[tps.rs](../../src/plugins/builtin/tps.rs)）是参考实现：delta 做 live
估算、usage 事件做修正、每回合落一个样本喂给空闲时的 sparkline。

它的上报契约值得照抄，因为那是页脚读数的一般规则，而不是 tps 的细节：

* **实时值随回合退场。** `tps:rate` 在 `TurnComplete` 时清除；冻结的数字
  会声称 agent 还在流式输出。
* **sparkline 是形态，不是数字。** 从第二个样本起才出现（单样本不成趋势），
  且不带任何数字；数值读数归 live rate 管。
* **会话边界清空计量。** `SessionStarted`、`SessionSwitched`、
  `ConversationCleared` 都丢弃历史——吞吐量必须描述屏幕上这段对话。
  `enabled` 开关保留，所以切换会话后计量继续运行。
* **一个回合的 token 按轮求和。** `TurnUsage` 只带最后一轮，而 decode span
  覆盖所有轮；计量器把每轮的 `RoundUsage` 存下来，后端不报分轮用量时再退回
  `TurnUsage`。

### 状态栏分段

插件提交**数据**，像素归宿主：`ctx.set_status(StatusSegment::new("rate",
"42 t/s"))` 会按键 upsert 存储为 `tps:rate` 的分段（`plugin:key` 完整键由
插件 id 解析），`ctx.clear_status("rate")` 移除它。色调——`Dim`、`Accent`、
`Warn`——映射到宿主调色板。没有任意绘制，这是有意的（见第 7 节）。分段
渲染在上下文读数与状态词之间，优先级由用户配置：

```toml
[status_bar]
plugin_segments = false             # 总开关（默认开启）
order = ["tps:rate", "tps:spark"]   # 显示优先级，从左到右
```

未列出的分段保持注册顺序；当前没有东西在显示的键会被忽略，所以过期的条目
没有代价。窄终端上，本就是最低优先级的右侧提示先退化，随后插件分段按自身
尾部顺序丢弃，宿主自己的读数与状态词永远不动。页脚每 90 ms 重绘一次，所以
插件一设置新文本，分段立即动起来——倒计时和仪表不需要宿主任何改动。

### 后台任务与插件配置口袋

`ctx.spawn(future)` 把任务交给宿主的运行时：它活得比派发更久，它发出的
副作用经插件的 bridge 到达 UI，宿主在退出时统一取消。future 里只留一个
`PluginBridge`——可克隆的副作用出口加取消 token：

```rust
fn on_startup(&self, ctx: &mut PluginContext<'_>) {
    let bridge = ctx.bridge();
    ctx.spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                    bridge.set_status(StatusSegment::new("clock", local_time()));
                }
                _ = bridge.cancelled() => break, // 合作式关闭
            }
        }
    });
}
```

长驻任务必须监听 `bridge.cancelled()`——取消是合作式的，宿主在其后的
`abort` 清扫并不优雅。任务 panic 会变成一条 transcript 通知，不会带倒 UI
循环。这就是同步钩子做不了的慢工作的正门：监控端的 WebSocket 服务器、
刷新循环、看门狗。

配置来自 `config.toml` 的插件口袋——宿主不解释任何键，与
`providers.toml` 的 `options` 给协议的自由度相同：

```toml
[plugins.my-plugin]
enabled = true
listen = "127.0.0.1:8737"
```

`ctx.plugin_settings()` 返回口袋对应的 `PluginSettings`，带
`option_str` / `option_u64` / `option_bool` 读取器与原始 JSON map。命名
未知插件 id 的表是**启动错误**——静默无操作会掩盖拼写错误——所以卸载
插件时请顺手删掉对应的表。

**监控插件端到端：**订阅 `EventSubscription::ALL`，把 `SNAPSHOT` 事件当作
权威 transcript，在 spawn 出的 WebSocket 服务器里尽力转发分批的增量，用
`ctx.submit_user_turn()` 支持远端提交 prompt。参考监控端（pi-monitor）的
线协议所做的每一件事都能用这些原语组合出来——宿主不需要任何新东西。

## 7. 设计笔记

**为什么编译期静态而不是动态加载 `.so`？** 动态加载需要自建 C ABI、版本
协商、panic/FFI 桥接——在当前规模下风险远大于收益。静态注册表给出确定性
（没有发现顺序、没有加载失败、没有运行时版本不匹配）、可读性、接近零的开销。
`inventory`/`linkme` 方式的分布式注册也被否决：链接器 section 行为和平台
相关的注册顺序比显式数组更差；若将来重拾，插件契约也不需要变。

**为什么是副作用？** 因为它们是纯数据，任何前端——TUI、无终端 runner、测试
——都可以按自己的方式应用，这正是插件不依赖 ratatui、整个系统不需要终端就能
测试的原因。

**有意设置的边界：** 钩子与 `on_event` 在前端线程上同步执行，必须快速返回
——慢工作属于 `run_tool` 或 `ctx.spawn` 出的任务，永远不属于钩子。没有沙箱
与崩溃隔离——插件以宿主权限运行（spawn 出的任务 panic 只波及该任务，那是
调度问题不是安全问题）。状态在进程内，随进程退出而消失。插件不能画自定义
widget、不能注册键位、不能碰终端：状态栏是"文本加色调"，transcript 是
cell，这些都是刻意的。外部插件沿用同一份契约，由生成的 wrapper crate 编译
进二进制（[local-plugins.md](../local-plugins.md)）。

**为什么状态栏是声明式分段而不是绘制回调？** 把 ratatui 类型放进契约
crate，意味着每次页脚升级都要让所有插件重新编译，还会把布局仲裁权交给
"最后画的那个插件"。文本加色调已经覆盖计量器与监控端需要的所有读数；而
draw 级 API 将来可以作为又一条带默认实现的 trait 方法加入而不破坏任何
东西——事件流就是这样与生命周期钩子并存的，规则相同。

**增长守则：** 每个框架特性必须是一个能撑起一**族**插件的原语——事件流、
状态分段、`spawn`、配置口袋——而不是为某一个想法加的一次性钩子。内置插件
就是验证场：如果一个提议中的 API 无法由其中某个插件来检验，那它多半还不
是原语。

## 8. 相关文档

- [local-plugins.md](../local-plugins.md) —— 安装与管理外部插件
- [multi-provider.md](../multi-provider.md) —— provider 实例、wire quirks、自定义协议
- [profiles.md](../profiles.md) —— profile 预设与 `tools` 过滤
- [../../README.md](../../README.md) —— 上手指南
- 设计笔记（中文）：[provider.md](../provider.md)、[session-id.md](../session-id.md)、[session-continue.md](../session-continue.md)
- 契约 crate 参考：[../../crates/cst-plugin-api/README.md](../../crates/cst-plugin-api/README.md)
