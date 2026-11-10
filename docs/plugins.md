# codest 静态插件系统设计

> 状态：已实现（`src/plugins/`），`/help`、`/clear`、`/quit`、`/status` 都是插件。
>
> 关键词：**编译期链接、静态注册、命令 + 钩子 + 数据化副作用、前端无关**。

## 1. 目标

codest 原本把 `/help`、`/clear`、`/quit` 硬编码在 `core::commands` 和 TUI 的
`match` 里。插件系统要解决的是：**在不把终端逻辑塞进 core 的前提下，让“新增一个
本地指令/生命周期行为”变成一件只改一个模块的事。**

具体目标：

| 目标 | 做法 |
| --- | --- |
| 指令集合可扩展 | 每个插件声明自己的 `CommandSpec`，由 `PluginHost` 汇总成一张表 |
| 指令解析只有一份 | `core::commands::CommandTable` 负责解析、别名、参数策略、帮助文本 |
| 生命周期可观察 | `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` |
| 插件不碰终端 | 插件只返回 `PluginEffect`，由 TUI 统一应用 |
| 可测试 | 不启动终端、不访问网络，直接构造 `PluginHost` 和假插件 |
| 零新增依赖 | 只用 `std` + 现有 `thiserror`，不引入 `inventory`/`linkme`/`libloading` |

非目标（v1 明确不做）：

- 运行时加载 `.so`/`.dylib`、脚本插件、热重载；
- 插件沙箱、权限系统、崩溃隔离；
- 插件直接绘制 ratatui 组件或注册键位；
- 异步钩子（钩子运行在 TUI 事件循环线程上，必须快速返回）。

## 2. 为什么是“静态”插件

这里的“静态”指**编译期链接**：插件是普通的 Rust 类型/值，和主程序一起编译，
注册表是一个 `static` 切片。不是“静态网站”的静态，也不是“插件文件放在某个目录
启动时扫描”。

```rust
pub static BUILTIN_PLUGINS: &[&dyn Plugin] =
    &[&help::HELP, &clear::CLEAR, &quit::QUIT, &status::STATUS];
```

收益：

1. **确定性**：注册顺序 = 钩子执行顺序 = 菜单顺序，没有发现顺序、加载失败、
   版本不匹配。
2. **可读性**：想知道二进制里有什么插件，看一个数组就够了。
3. **几乎没有运行时开销**：没有动态加载、没有注册发现、没有链接器 section
   魔法；注册表本身就是静态切片，`PluginHost` 构造只做一次小规模校验和索引。
4. **构造时校验**：重复插件 id、重复命令名/别名、非法命令名都会在
   `PluginHost::new` 返回错误（单元测试直接覆盖），启动即失败而不是运行时才暴露。
5. **类型安全**：插件直接使用 Rust 类型和现有 crate，不需要跨 FFI/JSON 边界。

代价：

- 新增插件必须改代码并重新编译，第三方无法“丢一个文件”就扩展；
- 不能按用户配置动态启停（可以在编译期用 feature/常量做裁剪）。

### 第三方接入点：契约 crate + 仓库外的生成 crate

“第三方无法扩展”这一点已经改观，但**不是**通过动态加载：契约类型被拆到
`crates/cst-plugin-api`（`Plugin`、`ProtocolFactory`、`ModelClient`、`Message`、
`ToolSpec`、`SseDecoder`、`AuthProvider` …），插件仓库依赖它、宿主依赖插件，
因此没有 Cargo 循环依赖。

用户侧接入是一条命令，**不改宿主源码**：

```bash
codest plugin install ../cst-provider-qoder
```

它把插件记进 `~/.codest/plugins.toml`，生成 `~/.codest/plugins/crate/`——一个
依赖宿主与各插件的独立 Cargo 包，里面唯一的实质内容是：

```rust
static LOCAL_PLUGINS: &[&dyn Plugin] = &[&cst_provider_qoder::PLUGIN];
```

然后在那里 `cargo build`。宿主侧提供两个接缝：`PluginHost::with_local(local,
filter)` 负责把注入的插件与内置插件合并校验，`codest::cli::run_with` /
`run_blocking` 负责把它接入启动流程。因为生成物住在仓库外，`git status` 永远
是干净的，也不需要 `git update-index --skip-worktree`。

`PluginHost` 仍按分组拼接 `[BUILTIN_PLUGINS, local]`，id / 命令名 / 工具名在两组
之间统一校验，所以本地插件不能覆盖内置插件；`PluginHost::shipped()` 只返回
内置插件，是测试用的稳定表。完整说明见 `docs/local-plugins.md`。

仍然没有动态加载：改插件就要重新编译宿主，这是编译期链接的代价。

为什么不用 `inventory` / `linkme`：它们能把注册分散到各个模块、由链接器收集，
但会引入链接器 section 行为、平台差异和不确定的注册顺序；对 codest 这种
“插件数量少、要求可读和确定性”的项目，显式静态数组更合适。`PluginHost` 的
输入只是一个切片，将来若需要分布式注册，可以把 `BUILTIN_PLUGINS` 换成
`inventory::iter` 的结果，而不改插件契约。

## 3. 分层与依赖方向

```text
┌──────────────────────────────────────────────────────────┐
│ tui                                                      │
│  App: parse → dispatch → apply_effects → render          │
│      PluginHost 的持有者，唯一接触终端和 AgentSession 的层 │
└───────────────┬──────────────────────────────▲───────────┘
                │ 调用                          │ PluginEffect（数据）
┌───────────────▼──────────────────────────────┴───────────┐
│ plugins                                                  │
│  Plugin trait / PluginHost / PluginContext / 内置插件      │
│  不依赖 ratatui，不直接操作 session                        │
└───────────────┬──────────────────────────────────────────┘
                │ 使用
┌───────────────▼──────────────────────────────────────────┐
│ core::commands                                           │
│  CommandSpec / CommandTable / ArgsPolicy / Invocation     │
│  纯数据 + 解析，不知道“插件”的存在                          │
└──────────────────────────────────────────────────────────┘
```

依赖方向严格是 `tui → plugins → core`，没有反向依赖。`core` 仍然可以被任何
前端驱动；`plugins` 也可以被非 TUI 前端复用。

## 4. 注册表与校验

内置插件在 `src/plugins/builtin/mod.rs`：

```rust
mod clear;
mod help;
mod quit;
mod status;

pub static BUILTIN_PLUGINS: &[&dyn Plugin] =
    &[&help::HELP, &clear::CLEAR, &quit::QUIT, &status::STATUS];
```

`PluginHost::new` 做三件事：

1. 校验插件 id 非空且不重复；
2. 收集每个插件的 `commands()`，构造 `CommandTable`；
3. 校验命令名/别名合法且全局不重复。

```rust
pub fn new(plugins: &'static [&'static dyn Plugin]) -> Result<Self, PluginHostError>
```

校验失败是启动错误，不是运行时警告——静态注册的问题应该在测试/启动时立刻暴露。
命令名规则：小写 ASCII 字母、数字、`-`、`_`、`?`（`?` 用于 `/help` 的 `/?` 别名）。

`PluginHost::builtin()` 在插件命令之外，还会把 `core::sessions::SESSION_COMMANDS`
（内置的 `/new`、`/rename`、`/sessions`，即多会话支持）合并进命令表，因此解析、
候选菜单和 `/help` 对它们一视同仁。这些命令不属于任何插件：它们是前端的常驻
行为，由 TUI 在 dispatch 给插件之前按命令名拦截处理。

## 5. 扩展契约

### 5.1 `Plugin` trait

```rust
pub trait Plugin: Sync {
    fn id(&self) -> &'static str;
    fn commands(&self) -> &'static [CommandSpec] { &[] }
    fn system_prompt(&self) -> Option<&'static str> { None }
    fn tools(&self) -> &'static [ToolSpec] { &[] }

    fn on_startup(&self, ctx: &mut PluginContext<'_>) {}
    fn on_user_message(&self, text: &str, ctx: &mut PluginContext<'_>) {}
    fn on_turn_complete(&self, message: &str, ctx: &mut PluginContext<'_>) {}
    fn on_conversation_cleared(&self, ctx: &mut PluginContext<'_>) {}

    fn run_command(
        &self,
        invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        Err(PluginError::new("not handled"))
    }

    fn run_tool(&self, call: &ToolCall) -> Result<String, PluginError> {
        Err(PluginError::new("not handled"))
    }
}
```

设计要点：

- **`&self` 而不是 `&mut self`**：插件以 `&'static dyn Plugin` 共享，注册表本身
  不需要可变性。需要状态的插件用 `Mutex`/原子类型做内部可变性（见第 8 节）。
- **默认实现**：只做钩子的插件不用写命令；只做命令的插件不用写钩子；不提供
  工具的插件不用写 `run_tool`。
- **`Sync`**：静态值必须可安全共享；这也阻止了插件在钩子里偷偷使用 `Rc` 等
  非线程安全状态。
- **`'static` 元数据**：`CommandSpec`、系统提示词片段和 `ToolSpec` 都是静态
  字符串，菜单和帮助文本可以直接借用，不需要克隆。

#### 工具：`tools()` + `run_tool()`

插件通过 `tools()` 声明工具（名称、描述、JSON Schema 参数文本），通过
`run_tool()` 执行模型发起的调用。core 侧的回合循环负责整条链路：请求带上
`tools` 字段 → 模型流式返回 `tool_calls` → worker 按名字派发到声明该工具的
插件 → 结果作为 `role: "tool"` 消息回传 → 继续本轮对话（上限
`MAX_TOOL_ROUNDS = 8` 轮，可被用户中断）。执行失败也回传给模型（内容为
`error: …`），模型可以选择重试或换路；transcript 上每次调用渲染为一个
`⚙` 单元。

`PluginHost` 在构建时校验工具名不重复（重复 → `PluginHostError::DuplicateTool`，
启动失败），并实现 core 的 `ToolExecutor` trait 作为 worker 的执行入口。
skills、workspace instructions 等上下文注入不需要专门的工具或钩子：作为
普通插件的 `system_prompt()` 片段即可，`/context` 会自动展示它们的出处。

### 5.2 `CommandSpec` 与参数策略

```rust
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub summary: &'static str,
    pub args: ArgsPolicy,
}

pub enum ArgsPolicy {
    None,      // 有参数就报错
    Optional,  // 参数可有可无
    Required,  // 至少一个非空白参数
}
```

解析发生在 `core::commands::CommandTable::parse`，因此所有前端对“什么算指令、
别名怎么匹配、参数是否合法”有一致行为。插件拿到的是已经校验过的
`Invocation { spec, args }`：

- `args` 是去掉首尾空白后的原始尾部文本，**保留内部空格和换行**；
- 参数如何进一步解析（按空格、按逗号、整段当 prompt）由插件自己决定。

### 5.3 `PluginContext`

插件能看到的只读信息：

| 方法 | 含义 |
| --- | --- |
| `command_table()` | 完整命令表，适合 `/help` 类插件 |
| `model()` | 当前后端标签 |
| `mode()` | `fullscreen` / `inline` |
| `transcript_len()` | 当前 transcript 单元数（近似值，用于状态展示） |
| `plugin_ids()` | 已加载插件 id |
| `context_window()` / `last_usage()` / `total_input()` / `total_output()` | token 用量（见 `/status`） |
| `system_sections()` | 组装后的系统提示词分段（`base` + `plugin:<id>`） |
| `effort()` | 随请求发送的推理等级（`None` 表示不发送） |
| `request()` | 请求去向与静态字段：protocol / base_url / endpoint / `body_overrides` |
| `tools()` | 插件声明的全部工具 |
| `message_count()` | 当前会话消息数的近似值 |

插件能请求的副作用：

| 方法 | 对应 `PluginEffect` |
| --- | --- |
| `notice(text)` | `Notice(String)` |
| `error(text)` | `Error(String)` |
| `report(block)` | `Report(ReportBlock)`：结构化报告块（粗体标题 + 粗体分组标题 + 强调色标签/暗色正文条目，`/context`） |
| `clear_conversation()` | `ClearConversation` |
| `quit()` | `Quit` |
| `set_composer(text)` | `SetComposer(String)` |
| `submit_user_turn(text)` | `SubmitUserTurn(String)` |

`PluginContext` 不暴露 `AgentSession`、`App` 或 `ratatui::Frame`。插件想“做事”，
只能描述意图；由前端决定怎么执行。

### 5.4 `PluginEffect` 的应用规则

TUI 按插件顺序、再按效果顺序应用：

| 效果 | TUI 行为 |
| --- | --- |
| `Notice` | 追加 `HistoryCell::Notice` |
| `Error` | 追加 `HistoryCell::Error` |
| `Report` | 追加 `HistoryCell::Report`（粗体标题 + 普通文本行） |
| `ClearConversation` | 清空 transcript + 调用 `session.clear_conversation()` |
| `Quit` | 设置 `quit`，下一帧退出 |
| `SetComposer` | 替换输入框内容并重置候选菜单 |
| `SubmitUserTurn` | 仅当 `!busy` 时提交；否则忽略 |

**重入保护**：`on_user_message` 在 `busy = true` 之后运行，
`on_turn_complete` 在 `busy = false` 之前运行，所以钩子里再请求
`SubmitUserTurn` 不会形成递归。命令处理时 agent 是空闲的，因此
“`/review` 这类命令提交一个 prompt”是支持的。

### 5.5 `PluginInfo` 与系统提示词

```rust
pub struct PluginInfo<'a> {
    pub model: &'a str,
    pub mode: &'a str,
    pub transcript_len: usize,
    pub plugin_ids: &'a [&'static str],
    pub context_window: Option<u64>,
    pub last_usage: Option<(u64, u64)>,
    pub total_input: u64,
    pub total_output: u64,
    pub system_sections: &'a [(String, String)],
    pub effort: Option<&'a str>,
    pub request: RequestFacts<'a>,
    pub tools: &'a [ToolSpec],
    pub message_count: usize,
}
```

启动时 TUI 先构造 `PluginHost`，再用
`host.system_prompt(base_prompt)` 把每个插件的 `system_prompt()` 片段用空行
拼到基础提示词后面，最后才创建 `AgentSession`。因此插件可以在第一轮之前
注入领域约束（例如“总是用中文回答”“这是代码审查场景”）。

### 5.6 错误处理

- `Plugin::run_command` 返回 `Err(PluginError)` → host 转成
  `PluginEffect::Error`，用户能看到，但不会中断应用。`run_tool` 的 `Err`
  同样不会中断：错误文本作为 tool 结果回传给模型，同时显示在 transcript 的
  `⚙` 单元里。
- 注册表错误（重复 id、重复命令、重复工具、非法名）→ `PluginHostError`，
  启动失败。
- 插件 panic **不会**被捕获：静态插件是可信代码，和主程序其他模块同级；
  需要隔离时应放在独立进程，而不是在这个系统里假装安全。

## 6. 执行流程

### 6.1 启动

```text
cli::run
  → PluginHost::builtin()?                 校验注册表
  → host.system_prompt(base)               拼接系统提示词
  → AgentSession::spawn_with_system_prompt
  → App::new(..., host)
  → host.startup(info) → apply_effects     插件的启动通知
  → event_loop
```

### 6.2 提交输入

```text
Enter
  → host.parse(text)
      ├─ Message  → submit_user_turn
      │              → 追加 User cell
      │              → busy = true
      │              → host.user_message(...) → apply_effects
      │              → session.submit_user_turn
      └─ Command  → host.dispatch_command(invocation, info)
                     → 找到拥有该命令的插件
                     → plugin.run_command(invocation, ctx)
                     → apply_effects(ctx.effects)
      └─ Invalid  → 追加 Error cell
```

### 6.3 回合结束与清空

```text
AgentMessage/TurnComplete
  → 保存最终 assistant 文本
  → finalize_live()
  → host.turn_complete(message, info) → apply_effects
  → busy = false

ConversationCleared
  → host.conversation_cleared(info) → apply_effects
```

`/clear` 是“命令 → 效果 → core 事件 → 钩子”的完整例子：
`Clear` 插件只请求 `ClearConversation`；TUI 清屏并通知 core；core 回
`ConversationCleared`；所有插件的 `on_conversation_cleared` 再运行。

## 7. 内置插件

| 插件 | 命令 | 作用 |
| --- | --- | --- |
| `help` | `/help` (`/h`, `/?`) | 渲染 `CommandTable` 的帮助文本 |
| `clear` | `/clear` | 请求清空 transcript 和上下文 |
| `quit` | `/quit` (`/exit`) | 请求退出 |
| `status` | `/status` | 展示 backend、模式、transcript 大小、命令数、插件列表 |

这些插件覆盖了三种典型形态：只读命令（`help`/`status`）、请求宿主动作
（`clear`/`quit`）、以及不需要命令的纯钩子插件（文档示例）。

`/context` 曾由 `prompt_debug` 插件承担，现在是前端自带命令
（`core::context` + `tui::context_view`），有两种形态：

- 会话还没有 transcript 消息时，输出原有的静态报告：组装后的 prompt
  （逐段出处）、请求配置、工具列表和 token 用量；
- 会话中则打开全屏上下文查看器：逐条列出 system + transcript，支持
  删除（整轮 tool 调用级联）、编辑（非 system 条目的 content）和撤销
  （checkpoint 逐步还原，重启后经 rollout 重放仍可撤销）。所有修改都
  是 op（`DeleteMessages` / `EditMessage` / `UndoContextEdit`），worker
  校验 tool_calls↔tool 结构后落盘并回 `ConversationSnapshot`。插入功能
  预留（core 已有 `insert_at` 原语）但未接 UI；上下文外置（把一段内容
  存到本地、上下文只留简述+索引地址）的存储契约在 `core::context_store`。

## 8. 新增一个插件

以“笔记”插件为例，演示参数、状态和多个命令：

```rust
// src/plugins/builtin/notes.rs
use std::sync::Mutex;

use crate::plugins::{
    ArgsPolicy, CommandSpec, Invocation, Plugin, PluginContext, PluginError,
};

pub static NOTES: Notes = Notes {
    notes: Mutex::new(Vec::new()),
};

pub struct Notes {
    notes: Mutex<Vec<String>>,
}

static COMMANDS: [CommandSpec; 2] = [
    CommandSpec::new("note", &[], "/note <text>", "save a note", ArgsPolicy::Required),
    CommandSpec::new("notes", &[], "/notes", "list notes", ArgsPolicy::None),
];

impl Plugin for Notes {
    fn id(&self) -> &'static str {
        "notes"
    }

    fn commands(&self) -> &'static [CommandSpec] {
        &COMMANDS
    }

    fn run_command(
        &self,
        invocation: &Invocation,
        ctx: &mut PluginContext<'_>,
    ) -> Result<(), PluginError> {
        match invocation.spec.name {
            "note" => {
                let mut notes = self
                    .notes
                    .lock()
                    .map_err(|_| PluginError::new("notes lock poisoned"))?;
                notes.push(invocation.args.clone());
                let count = notes.len();
                drop(notes);
                ctx.notice(format!("saved note #{count}"));
            }
            "notes" => {
                let notes = self
                    .notes
                    .lock()
                    .map_err(|_| PluginError::new("notes lock poisoned"))?;
                let text = if notes.is_empty() {
                    "no notes".to_string()
                } else {
                    notes
                        .iter()
                        .enumerate()
                        .map(|(index, note)| format!("{}. {note}", index + 1))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                drop(notes);
                ctx.notice(text);
            }
            _ => return Err(PluginError::new("unknown notes command")),
        }
        Ok(())
    }
}
```

然后在 `src/plugins/builtin/mod.rs` 加两行：

```rust
mod notes;
// ...
&[..., &notes::NOTES]
```

**不需要改 core、TUI、CLI。** 菜单、补全、帮助、参数校验会自动包含新命令。

## 9. 测试策略

插件系统的测试全部离线，分三层：

1. `core::commands`：解析、别名、大小写、参数策略、重复/非法命令名；
2. `plugins`：用假插件验证命令分派、钩子顺序、效果顺序、系统提示词拼接、
   重复插件 id/命令检测；
3. `tui::app`：验证 `/help`、`/status`、`/clear` 等效果确实落到 transcript，
   以及命令菜单数量/顺序。

```bash
cargo test          # 不访问网络
cargo clippy --all-targets -- -D warnings
```

新增插件时，至少补一个 host 层测试（分派后得到预期 `PluginEffect`）；
涉及 TUI 行为的再补一个 `App` 测试。

## 10. 边界与非目标

- **没有动态加载**：插件与主程序同生命周期，新增/修改插件必须重新编译。第三方
  插件通过 `cst-plugin-api` 契约 + `codest plugin install` 生成的仓库外 wrapper
  接入（见 `docs/local-plugins.md`），而不是放一个 `.so` 到某个目录。
- **没有隔离**：插件和主程序共享地址空间与权限；panic 不会被捕获。
- **没有异步钩子**：钩子在 TUI 线程同步执行，不能做阻塞 I/O；需要慢操作时
  应通过未来的异步 effect 或独立任务实现。
- **没有 UI 扩展点**：不能新增 HistoryCell 类型、不能画自定义 widget、不能
  注册键位；这些会破坏“插件不依赖终端”的边界，留到后续版本。
- **状态是进程内的**：`Mutex` 里的状态退出即丢失；持久化应由插件自己负责，
  或等待会话持久化能力。

## 11. 演进路线

1. **分布式注册**：用 `inventory`/`linkme` 替换静态数组，保持 `Plugin` 契约不变。
   注意 `cst-plugin-api` 拆分后，这个替换只影响宿主侧收集方式，插件的 `static`
   与 trait 契约都不用变。
2. **编译期开关**：按 Cargo feature 裁剪内置插件，做“最小 cst”。
3. **异步 effect**：`PluginEffect::Task` 或 `on_turn_complete` 的 async 版本，
   让插件做网络/文件操作而不阻塞渲染。工具调用这一半已先行落地——回合
   循环把每次工具执行放上阻塞池并对中断即时响应（见 `docs/profiles.md`
   §8）；剩余的是启动/命令 effect 的任务化，等真实插件需要阻塞 I/O 再做。
4. **工具调用插件**：把 `Op`/`EventMsg` 扩展到 tool call，让插件注册工具，
   这是从“本地指令插件”走向“agent 能力插件”的关键一步。
5. **渲染扩展**：允许插件提供纯数据 cell（标题、表格、diff），由 TUI 统一渲染，
   而不是让插件直接接触 ratatui。
6. **配置化启停**：在静态注册的基础上增加按名称/配置过滤的 host 构造方式。
   设计已并入 profile 方案——`tools` 字段驱动过滤，见 `docs/profiles.md`
   §4、§8；provider 侧的字段拼写差异同样走数据层（`[providers.wire]`
   quirk 字段，`docs/profiles.md` §5）。
7. **安装方式**：`codest plugin install` 已落地（本地路径 / git / crates.io，
   见 `docs/local-plugins.md`）。剩余的是等 `cst-plugin-api` 发布后去掉
   `[patch.crates-io]`，以及可选的插件签名与版本约束求解。
