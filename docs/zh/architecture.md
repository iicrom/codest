# 架构

[English](../architecture.md) | 简体中文

codest 的切分方式是：UI 从不直接与模型客户端对话。前端提交操作（`Op`），接收事件流（`Event`）；worker 持有会话、历史与网络流。这与 codex 的 SQ/EQ 是同一套生产者/消费者解耦，只是小到可以一口气读完。

```
┌──────────┐   Op (submit)    ┌────────────────┐  StreamItem   ┌────────────────────┐
│   TUI    │ ───────────────▶ │ session worker │ ────────────▶ │ ModelClient        │
│ tui/app  │                  │ core/session   │               │ core/openai_compat │
│          │ ◀─────────────── │                │ ◀──────────── │ (HTTP + SSE)       │
└──────────┘  Event (render)  └────────────────┘    chunks     └────────────────────┘
```

总线本身（[`src/core/protocol.rs`](../../src/core/protocol.rs)）只定义数据：`Op` 是前端可以发出的命令词表，`Event` 把一个 `EventMsg` 与触发它的 submission id 包在一起，`Submission` 是排队的那个二元组。`Op::UserTurn`、`Op::ContinueTurn`、`Op::ClearConversation`、`Op::NewSession`、`Op::SwitchSession`、`Op::RenameSession`、`Op::SwitchModel`、`Op::SetReasoningEffort`、`Op::SwitchProfile`、`Op::SetToolsEnabled`、`Op::InspectConversation`、`Op::DeleteMessages` / `Op::EditMessage` / `Op::UndoContextEdit` 三件套、`Op::Interrupt`、`Op::Shutdown` 全部走同一条通道。回复侧同样是单一枚举：回合进展（`TurnStarted`、`AgentMessageDelta`、`AgentThinkingDelta`、`AgentMessage`、`AgentToolCallStarted`、`AgentToolCall`、`TurnUsage`、`TurnAborted`、`Error`、`TurnComplete`）、会话账务（`SessionStarted`、`SessionSwitched`、`SessionRenamed`、`ConversationCleared`）与配置回显（`ModelSwitched`、`ReasoningEffortSwitched`、`ProfileSwitched`、`ToolsSwitched`、`ConversationSnapshot`、`ShutdownComplete`）。

## 从 codex 提取了什么

| codex 模块 | codest 对应模块 | 说明 |
| --- | --- | --- |
| `codex-rs/protocol`（SQ/EQ） | `src/core/protocol.rs` | `Op` / `Event` 消息总线；UI 不直接调用模型。 |
| `codex-rs/core` 会话循环 | `src/core/session.rs` | worker 持有全部会话历史与取消 token，产出事件。 |
| 多会话管理 | `src/core/sessions.rs` + `src/tui/session.rs` | 命令 spec 与每会话视图状态（`/new`、`/sessions`）。 |
| rollout 持久化 | `src/core/rollout.rs` | JSONL 会话落盘于 `~/.codest/sessions/`。 |
| `codex-rs/core/src/client.rs` | `src/core/model.rs` | 流式模型客户端抽象。 |
| Responses client | `src/core/openai_compat.rs` | 最小 OpenAI Chat Completions + SSE 解析。 |
| fake/OSS provider | `scripts/openai_test_server.py` | 本地 OpenAI 兼容测试服务（crate 内不内置 mock 后端）。 |
| `codex-rs/tui/src/app.rs` | `src/tui/app.rs` | 终端事件循环、渲染、输入。 |
| `bottom_pane/textarea.rs` | `src/tui/composer.rs` | 多行输入、光标、换行映射。 |
| `history_cell*` | `src/tui/chat.rs` | 用户/助手/错误/通知/中断单元格；thinking 与工具卡片。 |
| `markdown_render.rs` | `src/tui/markdown.rs` | Markdown → ratatui 行（高亮藏在 `highlight.rs` 后）。 |
| wrapping | `src/tui/wrap.rs` | Unicode 宽度感知换行与光标映射。 |

codex 的 TUI 超过 10 万行，core 带着大量沙箱/工具/审批基础设施并与 app-server 深度耦合，无法原样裁剪。codest 提取的是**可独立编译的架构骨架**，不是逐文件复制。

## 核心消息流

一个回合，从按键到渲染出的一行，经过三个模块。

**1. 提交。** TUI 调用 `AgentSession::submit`，给操作打上进程内自增的 id（`sub-1`、`sub-2`……），把 `Submission` 推入无界通道。`AgentSession` 只向前端暴露两个原语：`submit(op) -> String` 与 `next_event().await -> Option<Event>`。为某个 submission 产出的事件都带它的 id，并发的操作因此可以区分。

**2. worker 接单。** [`src/core/session.rs`](../../src/core/session.rs) 里的 `run_worker` 持有全部会话、rollout writer 与工具集。收到 `Op::UserTurn` 后，它把文本追加进当前 `Conversation`、记入 rollout、为当前选择构建客户端、调用 `ModelClient::stream_chat(messages, options, cancel)`，再把得到的流移入 `ActiveTurn`。`TurnOptions` 携带本回合的 effort、声明的工具 spec 与当前会话 id——都在这一刻解析，所以回合之间的 `/model` 或 `/tools` 改动会在下个请求生效，不需要碰客户端。在这之前还有两道闸：完全没选后端，以及半配好的端点（只给了 `base_url` 或 `api_key` 之一）——两者都用指引代替注定失败的请求。

**3. 流式。** `ModelClient`（[`../crates/cst-plugin-api/src/model.rs`](../../crates/cst-plugin-api/src/model.rs)）返回 `ModelStream`，是一个 `Result<StreamItem, ModelError>` 的 boxed 流，而不是整份响应。`run_turn` 在流与回合的取消 token 之间 `select!`，把每个 item 转成事件：`StreamItem::Delta` → `AgentMessageDelta`，`StreamItem::Thinking` → `AgentThinkingDelta`，`StreamItem::ToolCalls` 留给工具轮，`StreamItem::Usage` 记下来供 footer 使用。工具调用随后逐个执行——执行前 `AgentToolCallStarted`，执行后 `AgentToolCall`——结果追加进会话，下一轮再重新开流。轮次循环在模型不再调用工具时结束；设置了 `max_tool_rounds` 并命中上限时，以 "tool round limit reached" 报错结束。

某一轮工具调用之前流出的文本会先定型，连同该轮的推理一起，保证 transcript 的顺序（文本 → 调用 → 结果），回放也能重建同样的形状。`StreamItem` 是 `#[non_exhaustive]`，插件协议新增变体不会破坏这次构建，只会被忽略。

**4. 定型。** `finalize_complete` 把本轮的推理与助手文本写入 rollout，把文本推进会话，发出 `AgentMessage`，再发 `TurnUsage` 与 `TurnComplete`。前端只在事件流上渲染；它不直接读会话状态，唯一的例外是 `Op::InspectConversation`，回答是一个 `ConversationSnapshot`。

**取消。** 每个回合创建一个 `CancellationToken` 并交给客户端共享。前端的 `Esc` 路径调用 `InterruptHandle::interrupt`，取消正在进行的回合的 token；worker 的 `select!` 偏向取消分支，因此立刻定型：模型要求但结果永远没到的工具调用会用一条显式的 `interrupted` 工具结果封口（保证下一个请求的线协议合法），已流出的助手文本被提交，回合以 `TurnAborted` 加 `TurnComplete` 结束。已经跑在阻塞线程池上的工具停不下来——它的结果被丢弃。`Op::Interrupt` 也在总线上，但 TUI 直接走句柄，取消因此不必排在它要取消的那个回合后面。同一个 `InterruptHandle` 在每回合开始时装配、在每条定型路径上清空，空闲时的 `Esc` 因此是 no-op，而不是一次游走的取消。

## 状态归属

只有 worker 改动会话状态。这是整套设计的基本原则，也解释了为什么有些东西看起来是绕了一圈。

| 状态 | 持有者 | 前端如何看到 |
| --- | --- | --- |
| 会话历史（系统提示词 + 消息） | worker，每会话一个 `Conversation` | 请求时或上下文变更后收到 `ConversationSnapshot` |
| 会话列表 | worker（只追加，按创建顺序编号） | `SessionStarted` / `SessionSwitched` / `SessionRenamed` |
| 当前模型选择 | worker 的 `ModelBackend`（`Fixed` / `Pooled` / `Empty`） | `ModelSwitched`，以及启动路由 |
| 推理等级 | worker，按运行 | `ReasoningEffortSwitched` |
| 启用工具集 | worker，按会话 | `ToolsSwitched`，变更时发出、切换会话时重新同步 |
| rollout 文件 | worker 的 `Rollouts`（每会话一个 writer 槽位） | 不可见；启动时回放 |
| 渲染出的 transcript、滚动与折叠标志 | 前端，每会话一个视图 | worker 不可见 |
| 进行中的模型流 | worker 唯一的 `ActiveTurn` | `AgentMessageDelta` / `AgentThinkingDelta` |

有两个推论值得点名。其一，运行中切换会话会恢复该会话的模型与工具集，因为两者都按会话记录、在它重新激活时重新应用；这些事件先于确认用的 `SessionSwitched` 到达，前端镜像在它响应时已经一致。其二，前端维护一份配置镜像（footer 标签、菜单勾选），并把 worker 的事件当作权威——乐观提示可能先出现，但由事件定案。

一个 worker 同一时刻只流一个回合。提交是 FIFO 的，所以回合进行中提交的 `/model`、`/effort`、`/profile`、`/tools` 落在**回合之间**：正在跑的回合保留自己的 `TurnOptions`，下一回合才吃到新值。配置类操作因此被描述为"回合之间生效"，而不是"立即生效"。

## 前端循环

`tui/app.rs` 跑一个 `tokio::select!`，五个输入：crossterm 的 `EventStream`（按键、鼠标、粘贴、resize）、worker 的 `next_event()`、后台 `/provider` 结果通道、定时 notice 通道（模型发现通过它回报），以及一个 90 ms 且跳过错过刻度的 ticker。循环每轮开头重绘，所以任何一个输入都能产生一帧。ticker 的存在是为了让转轮动起来、让限时 footer 消息按时过期，而不是按需渲染。

插件 effect 就在这个循环里应用。插件从不接触终端：它返回八种 `PluginEffect` 之一——notice、error、report block、清空会话、退出、设置输入框、提交回合、续跑回合——前端把它们各自翻译成自己的动作。因为 effect 是数据，同一份插件代码可以在任何前端下运行，插件层也不需要依赖 ratatui。

渲染按事件与 tick 进行：应用为 worker 的每个会话保留一个 `SessionView`，装着渲染好的单元格以及滚动、折叠标志，只绘制当前激活的那个。`/provider` 的工作与模型发现跑在独立任务上、通过通道回报，慢探测永远不会阻塞渲染线程。

## 启动装配

`src/cli.rs` 是装配点，运行在任何 worker 存在之前。它加载 `profiles.toml`（保留 `--profile` 预设）、按该 profile 的工具过滤器构建 `PluginHost`、加载 `config.toml` 的全局默认值，再从 `providers.toml` 构建 `ModelPool`（没有该文件时用 flag 与环境变量合成单端点）。随后解析启动选择——`--provider`/`--model`，否则激活 profile 的 `default`，否则 `config.toml` 的 `default`，否则第一个实例的第一个模型——并先验证它能构建出客户端，端点坏掉在启动时就失败，而不是等到第一回合。最后把 `--continue` / `--resume` 映射成 `RestoreMode`，全部交给 `tui::run`。

加新设置时有两个细节。profile 的**装配期**值（工具过滤器、提示词追加）在这里应用一次；它的**会话期**值（选择、effort）则装配进 worker，之后仍可通过 op 改变。另外 `ModelBackend` 有三种形态——`Fixed` 供测试与简单宿主、`Pooled` 为多 provider 运行时、`Empty` 是"什么都没配"的起点，此时回合以指引作答，直到 `/model` 切换把它迁移走。

## 项目结构

```
codest/
├── Cargo.toml
├── LICENSE
├── README.md                  # 本文件（英文）
├── README.zh-CN.md            # 中文版
├── crates/
│   └── cst-plugin-api/        # 插件契约 crate（打破 Cargo 依赖循环）
├── docs/                      # 用户文档（英文）+ 设计笔记（中文）；docs/zh/ = 中文版
├── scripts/                   # 离线测试服务 + pty 冒烟测试
└── src/
    ├── main.rs                # codest 主入口
    ├── bin/cst.rs             # cst 别名入口
    ├── cli.rs                 # clap CLI + 启动装配
    ├── lib.rs
    ├── core/
    │   ├── protocol.rs        # Message / Op / Event 总线
    │   ├── commands.rs        # 命令表 / 解析
    │   ├── sessions.rs        # /new /rename /sessions 命令 spec
    │   ├── reasoning.rs       # /effort /thinking spec + 推理等级
    │   ├── providers.rs       # 协议工厂 / 注册表 / 模型池 / /model
    │   ├── config.rs          # config.toml / providers.toml / profiles.toml + $ENV 模板
    │   ├── context.rs         # /context 指令
    │   ├── context_store.rs   # 上下文浏览/编辑状态
    │   ├── tools.rs           # 工具派发
    │   ├── tools_switch.rs    # /tools 会话开关
    │   ├── settings.rs        # /settings 指令 + config.toml 写入
    │   ├── conversation.rs    # 线性会话历史
    │   ├── model.rs           # ModelClient trait + StreamItem + 错误类型
    │   ├── openai_compat.rs   # HTTP + SSE 流式客户端（openai-compat 协议）
    │   ├── sse.rs             # 共享 SSE 帧解析
    │   ├── tokens.rs          # token 统计
    │   ├── rollout.rs         # 会话持久化（JSONL rollout）
    │   ├── session_id.rs      # UUIDv7 会话 id
    │   ├── echo_model.rs      # 进程内 echo 模型（测试 + `test` 协议后端）
    │   └── session.rs         # agent worker + AgentSession
    ├── plugins/
    │   ├── mod.rs             # Plugin trait 宿主 / effects / 协议收集
    │   ├── install.rs         # codest plugin install|uninstall|list|build
    │   └── builtin/           # 静态注册表：help、clear、quit、status、continue、tps、tools/{shell,bash,pwsh,fs,truncate}
    └── tui/
        ├── app.rs             # 事件循环 / fullscreen & inline 渲染 / 菜单
        ├── session.rs         # 每会话视图状态
        ├── chat.rs            # 历史单元格（含 thinking 与工具卡片）
        ├── composer.rs        # 输入编辑器
        ├── provider_wizard.rs # /provider 向导
        ├── context_view.rs    # /context 查看器
        ├── settings_view.rs   # /settings 界面（字段 → 行的映射）
        ├── screen.rs          # 整帧界面的框架：头部/分区条/分隔线/提示/帮助栏 + 滚动
        ├── cards.rs           # 卡片边框、指针 gutter、行与值列
        ├── inline_edit.rs     # 列表界面共用的一行编辑器
        ├── markdown.rs        # Markdown 渲染（pulldown-cmark）
        ├── highlight.rs       # 代码高亮（syntect → TokenKind）
        ├── theme.rs           # 颜色
        └── wrap.rs            # 换行与光标映射
```

有三条边界值得点名。`crates/cst-plugin-api` 是独立 crate，插件可以只依赖契约而不依赖二进制；`src/core/model.rs` 只是从它那里再导出 trait。`src/core` 不知道 ratatui，`src/tui` 不知道 HTTP。`src/plugins` 位于两者旁边，只通过契约类型与它们接触。

## 扩展点

| 你想做的事 | 看这里 |
| --- | --- |
| 增加 slash 指令、钩子、工具、协议或登录流程 | [plugins.md](../plugins.md) —— `Plugin` trait 与它的 effects。 |
| 支持另一种端点的线协议或 quirk | [multi-provider.md](../multi-provider.md) —— 实例、wire quirks、协议工厂。 |
| 打包模型 + effort + 工具 + 提示词预设 | [profiles.md](../profiles.md) —— 预设与选择优先级。 |
| 安装来自其他仓库的插件 | [local-plugins.md](../local-plugins.md) —— 仓库外的 wrapper crate。 |

三份契约文件承载了接缝：[`../crates/cst-plugin-api/src/plugin.rs`](../../crates/cst-plugin-api/src/plugin.rs)（`Plugin` trait 与 effects）、[`../crates/cst-plugin-api/src/model.rs`](../../crates/cst-plugin-api/src/model.rs)（`ModelClient`、`StreamItem`、`TurnOptions`）、[`../crates/cst-plugin-api/src/sse.rs`](../../crates/cst-plugin-api/src/sse.rs)（插件协议复用的共享 SSE 解码器）。`src/core/model.rs` 与 `src/core/sse.rs` 是后两者的薄再导出，保留历史模块路径可用。

## 相关文档

- [tui.md](../tui.md) —— 事件如何变成像素：模式、Markdown、卡片。
- [configuration.md](../configuration.md) —— provider 与 profile 在哪里声明。
- [getting-started.md](../getting-started.md) —— 从安装到第一次会话，含完整 CLI 表。
- [../src/core/protocol.rs](../../src/core/protocol.rs) —— 完整的 `Op` / `EventMsg` 词表。
- [../src/core/session.rs](../../src/core/session.rs) —— worker 循环与回合定型。

[← 返回 README](../../README.md)
