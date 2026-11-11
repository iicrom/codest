# codest `/continue`：异常中断后的会话续跑

> 状态：已实现（`src/plugins/builtin/continue.rs`、
> `crates/cst-plugin-api/src/plugin.rs`、`src/core/protocol.rs`、
> `src/core/session.rs`、`src/core/conversation.rs`、`src/core/rollout.rs`）。
> `/continue` 是内置插件命令：不切换会话、不追加用户消息，直接从当前
> transcript 原样续跑。
>
> 关键词：**不切换、不重发、interrupted 封口、abort 与回放双路修复、append-only 幂等**。

## 1. 问题

会话以 append-only JSONL rollout 持久化（见 `docs/session-id.md`），但恢复入口
只有启动期的 `--continue` / `--resume`。进程已经起来之后，一次异常中断留下的
状态没有任何办法接着跑，只能把上一轮内容重新打一遍。中断留下两类"悬空"：

| 中断点 | transcript 尾部 | 后果 |
| --- | --- | --- |
| 流式中崩溃 | 悬空 user 消息（无回复） | 无害，但需要人工重新驱动 |
| 工具执行中崩溃 / Ctrl+C | `AssistantToolCalls` 无 results | **wire 非法**：chat-completions 端点拒绝下一次请求 |
| 流式中 Ctrl+C | 部分回答 | 需要人工续写 |

工具悬空尤其严重：calls 落盘于执行前（`session.rs` 的工具循环先 append 再
execute），结果在其后逐个写入；`finalize_aborted` 原本只提交部分文本，既不修
conversation 也不修 rollout——打断后哪怕直接发一条新消息也会 400。

## 2. 语义

- **不切换会话**：作用对象永远是当前活跃会话。切换用 `/sessions`。
- **不追加用户消息**：没有对应文本可"重发"——transcript 尾部已经有一条悬空
  user 消息时，再 append 一份只会污染上下文。续跑 = 用当前 transcript 原样
  发起一次模型请求。
- **不解析"是否中断过"**：尾消息是完整的 assistant 回答时同样续跑。这是显式
  的用户指令（"接着说"），语义可预期；判断"是否真的中断"无法从 transcript
  形状可靠得出，不做。

## 3. 恢复行为（按 transcript 尾状态）

| 尾部 | 行为 |
| --- | --- |
| `User` | 直接续跑，模型回答悬空的那条 |
| `Assistant(tool_calls)` 无 results | 不会到达这里：回放与 abort 双路都已封口（见 §4） |
| `Tool`（interrupted 结果） | 直接续跑，模型对 "interrupted" 做出反应 |
| `Assistant`（完整或部分文本） | 直接续跑，模型接着写 |
| 空 transcript | 拒绝：`Error("nothing to continue: …")` **+ `TurnComplete` 成对发送** |

## 4. interrupted 封口

补写的工具结果内容是
`Conversation::INTERRUPTED_TOOL_RESULT`（"interrupted: the session ended
before this tool call finished"），模型在下一轮读到它可以做出反应，而不是
请求直接被拒。封口在两处发生，任何一路都让 transcript 回到 wire 合法：

1. **abort 时**（live）：`finalize_aborted` 开头调用
   `Conversation::close_unanswered_tool_calls(conversation.messages_mut())`，
   补写结果同时进入 conversation 与 rollout（`RolloutItem::ToolResult`），
   文件与内存保持一致。
2. **回放时**（resume）：`rollout.rs` 的 `load_session` 在构造
   `LoadedSession` 前对 messages 调用同一函数。覆盖两类文件：本改动之前遗留
   的悬空文件、崩溃时没走到 abort 路径的截断文件。

文件永远不改写（append-only）：悬空行留在盘上，封口是每次回放的纯内存行为，
因此天然幂等；之后任何一行结果补进文件，回放自动变为完整轮，无需再补。
`close_unanswered_tool_calls` 的遍历方式镜像
`Conversation::validate_tool_structure`，所以该函数会拒绝的每一个缺口都会被
补上；`SessionView::restored` 按 call id 回填工具卡，恢复的会话里这张卡直接
显示 interrupted 文本。

## 5. 实现链路

```
/continue (内置插件 continue.rs)
  → PluginEffect::ContinueTurn        插件只发效果，不碰会话（crates/cst-plugin-api）
  → App::apply_effects                TUI 应用：idle 才执行；无路由给指引；
                                      置 busy、开 live 缓冲；不触发 user_message 钩子
  → AgentSession::continue_turn()
  → Op::ContinueTurn                  worker：路由守卫同 UserTurn；不 push user；
                                      snapshot() 建流；之后 run_turn 流式 +
                                      工具循环 + finalize 全复用
```

要点：

- **命令解析/菜单//help 免费获得**：spec 注册在 `BUILTIN_PLUGINS`
  （`builtin/mod.rs`，`mod r#continue;` —— `continue` 是 Rust 关键字），
  与 `help`/`clear`/`quit`/`status` 同列。
- **busy 期间静默忽略**：与 `SubmitUserTurn` 效果一致；且 `try_submit` 在
  busy 时本来就拦截一切命令输入，`/new`、`/sessions` 同此待遇。
- **worker 拒绝必带 `TurnComplete`**：TUI 在效果应用时就置了 busy（与
  `submit_user_turn` 相同的即时反馈），拒绝路径若只发 `Error`，busy 永远
  无法复位。
- **不触发 `plugins.user_message`**：这不是用户消息；`turn_complete` 钩子
  正常触发（它是一次真实的回合结束）。
- **事件路由零改动**：流式增量落入当时活跃的 `view.live`，会话切换语义与
  普通 turn 完全一致。

## 6. 与 `--continue` 的分工

| | `--continue` / `--resume` | `/continue` |
| --- | --- | --- |
| 时机 | 启动期 | 任意时刻 |
| 对象 | 最近保存的会话（激活它） | 当前活跃会话 |
| 动作 | 恢复视图/模型/工具集 | 补一次模型请求 |
| 是否换会话 | 是 | 否 |

崩溃后的典型流程：重启（默认开新会话）→ `/sessions` 切回中断的会话 →
`/continue` 接着跑。`--continue` 启动的会话同样可以直接 `/continue`。

## 7. 边界

- 空会话（`/clear` 后、全新会话）拒绝，无副作用。
- 插件可以把 `ContinueTurn` 效果用在任意时刻（比如启动钩子里自动续跑），
  守卫与手输 `/continue` 完全相同。
- rollout 文件中的悬空行永久保留；回放修复不影响磁盘内容。
- 与 `/context` 删除的交互：live 删除由 `validate_tool_structure` 保证不产生
  悬空；因此悬空只来自中断，回放修复覆盖全部来源。

## 8. 测试

- `src/core/conversation.rs`：悬空轮补全 / 完整轮不动 / 只补缺失 id。
- `src/core/rollout.rs`：截断文件回放封口，文件 append-only 且后续结果行使
  回放自愈。
- `src/core/session.rs`：空会话拒绝（Error+TurnComplete 成对、无模型请求）；
  续跑不追加 user 消息；工具中打断后 conversation 与 rollout 都含 interrupted
  结果。
- `src/plugins/mod.rs`：shipped 命令表顺序、`/continue` →
  `[PluginEffect::ContinueTurn]`。
- `src/tui/app.rs`：无模型给指引不置 busy；有模型置 busy 且不追加 User cell；
  busy 期间忽略。
