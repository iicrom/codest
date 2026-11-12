# 会话

[English](../sessions.md) | 简体中文

一个会话就是一段对话：消息历史、thinking 卡片、工具调用，以及属于它的会话级开关（模型、effort、工具、profile）。会话可以命名、列出、在进程内切换，并以 append-only JSONL rollout 的形式落盘，因此能跨次启动存活。

## 会话指令

| 指令 | 别名 | 行为 |
| --- | --- | --- |
| `/new` | — | 归档当前对话，开启新的空会话。 |
| `/rename <name>` | — | 重命名当前会话（默认名取第一句用户输入）。 |
| `/sessions` | `/resume` | 打开会话列表，切换并继续以前的会话。 |
| `/continue` | — | 不追加用户消息、直接续跑当前会话——崩溃/中断后的恢复入口。 |

## 行为

- **同一时刻只有一个会话在生成。** 只有活动会话可以是 busy 的；会话之间没有并行生成。
- **生成中丢弃输入。** 输入框仍可编辑，但提交（消息或指令）在回合结束前被丢弃。
- **会话指令需要回合边界。** 回合运行期间不接受 `/new`、`/sessions`、`/continue`——先按 `Esc` 中断，再执行指令。
- **切换会恢复上下文。** 在 `/sessions` 中选中会话即恢复其对话；core 还会重新应用该会话上次使用的模型选择与启用工具集（尽力而为——已失效的选择保留当前模型），footer 的用量计数由记录的回合初始化。op 是 FIFO 的，因此下一轮用户消息会带着恢复后的上下文被回答。选中当前活动会话则只关闭菜单。
- **`/clear` 只作用于当前会话。** 它只清空活动会话的上下文与 transcript；其他会话以及输入框的输入历史都不受影响。
- **会话列表显示名字、单元格数量与 `current` 标记。** 行带序号，活动会话带标记，名字按宽度截断。
- **默认命名。** 会话在 `/rename` 之前以第一句用户输入命名；名字记录在 rollout 中，因此跨重启存活。

## 会话生命周期

会话指令是前端行为而非模型回合——它们在本地派发，不会作为用户消息追加到对话里。

| 动作 | 效果 |
| --- | --- |
| `/new` | 视图立即切到新会话并通知 core；由于 op 是 FIFO 的，下一轮落在新对话上。旧会话仍留在列表与磁盘上。 |
| `/rename <name>` | 设置显示名。换行被压成空格（列表每会话一行），transcript 打印一条通知，重命名追加进 rollout 因而跨重启存活。 |
| `/sessions` | 对已记录会话打开模态选择器，从活动会话开始。`↑`/`↓` 移动，`Enter` 确认，`Esc` 关闭；打字与 `Ctrl+C` 仍走各自的正常处理。 |
| `Esc`（选择器中） | 关闭菜单，不切换。 |

- 选择器只对导航键是模态的：`↑`/`↓` 移动选中项，单独的 `Enter` 确认。确认切换后，该会话的 transcript 追加一条 `Session resumed` 通知。
- `/clear` 重置活动会话的上下文与 transcript，但保留其名字、文件与输入框的输入历史。它同时重置用量计数，因为被丢弃的回合不再描述当前上下文；回放 rollout 时也按同样规则重置。
- rollout 写入器位于 core，与 codex 一致：会话真正发生变化时（用户消息、完成的助手消息、工具调用与结果、clear、rename、各种切换）worker 追加一行。

选择器每个会话一行——标记、从 1 开始的序号、名字（`/rename` 名，否则第一句用户输入，按宽度截断）、单元格数量，以及活动会话上的 `current` 标记：

```text
▸ 1 · refactor the rollout loader · 24 cells · current
  2 · why does the SSE parser drop frames · 9 cells
  3 · session 3 · 2 cells
```

## 持久化（rollout）

- 会话按工作区存储在 `~/.codest/sessions/<工作区哈希>/rollout-<uuid>.jsonl`。
- 工作区哈希是**启动目录的 FNV-1a**（16 位十六进制），因此每个项目保有自己的一组会话。
- uuid 是 **UUIDv7**，文件名排序即创建顺序；同一个 id 也是 `session-id` 请求头的取值。
- 文件是 **append-only JSONL**：每行是一个时间戳加一条记录，首行是会话元数据。记录类型用 `snake_case` 标记：

| 行类型 | 记录内容 |
| --- | --- |
| `session_meta` | 会话 id、创建时间、工作区 `cwd`、系统提示词与初始名字——永远是首行。 |
| `user_message` | 提交的提示词。 |
| `assistant_message` | 完成的助手回复。 |
| `assistant_reasoning` | 一张 thinking 卡片（正文加流式时长）；纯显示元数据。 |
| `assistant_tool_calls` | 整轮的工具调用，回放为一条带 `tool_calls` 的助手消息。 |
| `tool_result` | 单个工具的输出，回放为回答对应 `tool_call_id` 的 `role: "tool"` 消息。 |
| `cleared` | 会话内的 `/clear`（保留系统提示词）。 |
| `renamed` | `/rename`；回放时最后一个生效。 |
| `model_switched` / `reasoning_effort_switched` | 模型与 effort 变更——归属元数据。 |
| `tools_switched` | 会话的启用工具集；最后一个生效，缺省表示声明的工具全部开启。 |
| `profile_switched` | 活动 profile 的名字。 |
| `turn_usage` | 一个完成回合的输入/输出 token 数，以及服务该回合的模型。 |
| `context_deleted` / `context_edited` / `context_restored` | `/context` 查看器中的编辑：下标与被移除内容（删除的逆操作），以及撤销标记。 |

- **回放是忠实的。** 从文件重建对话可精确还原——包括卡片折叠/展开状态。thinking 记录是显示元数据：它们会被回放成卡片，但永远不会重新进入模型上下文。
- **id 立即分配，文件惰性创建。** 会话创建时就分配 id（第一轮请求即带 session 头），文件在第一条用户消息时才创建。空会话不创建文件。
- **写入是尽力而为的。** 落盘失败不影响对话；错误写到 stderr，回合继续。
- **文件名不是 uuid 的文件不是会话。** 列表会跳过它，包括旧版本用 hex 纳秒命名的文件。
- **回放容忍未来版本。** 未知记录类型在读取时跳过，因此新版本可以扩展格式而不破坏旧读者；一条丢了所属消息的 reasoning 行（文件在这两行之间被截断）会被丢弃，而不是显示在无关回合的上方。
- **根目录可迁移。** `CODEST_HOME` 替换 `~/.codest`，整个 `sessions/` 树随之迁移。

```text
~/.codest/sessions/
└── 3f9c1a77b2d04e18/                  # 启动目录的 FNV-1a
    ├── rollout-0192f3c1-...-a1b2.jsonl   # 先创建
    └── rollout-0192f3c1-...-c3d4.jsonl   # 后创建（UUIDv7 顺序 = 文件名顺序）
```

```jsonl
{"timestamp":"2026-01-04T12:00:00Z","type":"session_meta","version":1,"id":"0192f3c1-...","created_at":"2026-01-04T12:00:00Z","cwd":"/work/project","system_prompt":"..."}
{"timestamp":"2026-01-04T12:00:09Z","type":"user_message","message":"why does the parser drop frames?"}
{"timestamp":"2026-01-04T12:00:14Z","type":"assistant_reasoning","text":"...","duration_ms":4200}
{"timestamp":"2026-01-04T12:00:15Z","type":"assistant_message","message":"Because ..."}
```

- **没有并发写保护。** 同一工作区上的两个 codest 实例可以向不同文件交错追加，没有仲裁。此外也没有归档/删除/搜索，没有跨工作区迁移。

## 启动与恢复参数

```bash
codest --continue    # 直接激活本工作区最近一次保存的会话（-c 等价）
codest --resume      # 同上，但先打开会话选择菜单
```

| 参数 | 行为 |
| --- | --- |
| `--continue`、`-c` | 加载本工作区最近一次保存的会话并直接继续。 |
| `--resume` | 加载本工作区全部已保存会话，并在启动时打开选择器。 |

两个参数互斥；都不给时，codest 以全新会话启动（新 id，且在首条消息前没有文件）。

恢复的会话会带回：

| 恢复项 | 说明 |
| --- | --- |
| 对话与 thinking 卡片 | 在渲染任何内容之前从 rollout 回放。 |
| 模型选择 | 该会话上次使用的选择，在首次渲染前采用。 |
| 启用工具集 | 最后一次 `/tools` 的状态，并同步到前端。 |
| footer 用量 | 由记录的回合初始化，首次新回合之前就能看到上下文大小。 |
| profile **标签** | 会话上次使用的 profile 显示为活动；其预设不会重新应用——恢复的会话恢复的是自身状态。 |

没有已保存会话时，`--continue` 与 `--resume` 都从全新会话开始。

## `/continue`：进程内恢复

- `/continue` **不重发消息**，直接续跑当前会话——按现状驱动 transcript，因此模型从停下的位置精确接续。它不触发 user-message 钩子。
- 被崩溃或 `Ctrl+C` 打断的工具轮会补上一条显式的 `interrupted` 工具结果（`interrupted: the session ended before this tool call finished`），保证下一次请求的 wire 形状合法：每个 `tool_call` 都有匹配的 `role: "tool"` 回复，模型可以对中断做出反应，而不是请求被直接拒绝。
- 封口发生在两处，任何一路都能让 transcript 回到 wire 合法：**abort 时**补写的结果同时进入 conversation 与 rollout，内存与文件保持一致；**回放时**同一函数在内存中执行，覆盖崩溃且没走到 abort 路径的截断文件。
- 文件本身**永远不改写**——它是 append-only 的，因此回放封口天然幂等；遗留的悬空行留在盘上，之后任何一行结果补进文件就自动变成完整轮。
- transcript 为空时拒绝续跑并报错，而不是发起回合。
- 只在空闲时执行：生成中的 `/continue` 被忽略。没有可用路由时，打印与普通发送相同的添加 provider 指引。

完整恢复语义（transcript 尾部各情形、abort 与回放双路）见 [session-continue.md](../session-continue.md)（中文设计笔记）。

## 会话 id 与 `session-id` 头

- 每个会话都有 UUIDv7 id。默认情况下每个请求以 `session-id` 头携带它（codex 的拼写），按会话路由或缓存的端点无需配置即可识别。
- 头名可按实例用 `wire.session_header` 配置，也可按模型用 `models.session_header` 覆盖实例设置。

按实例关闭或改名：

```toml
[[providers]]
name = "strict-endpoint"
base_url = "https://example.test/v1"
wire = { session_header = "none" }          # 该端点拒绝未知头

[[providers]]
name = "gateway"
base_url = "https://gateway.test/v1"
wire = { session_header = "x-session-id" }  # 改用网关自己的拼写
```

- `"none"` 让实例（或模型）完全关闭该头；空名或纯空白名是配置错误，而不是另一种关闭方式。
- 模型级 `session_header` 也可以在实例关闭的情况下重新开启。
- 静态 `headers` 无法钉住该头：逐轮的会话 id 在它们之后应用，因此手写的同名条目会被覆盖，与 `/effort` 覆盖 `body_overrides` 的优先级一致。

eager-id/lazy-file 拆分与兼容性规则见 [session-id.md](../session-id.md)（中文设计笔记），周边文件参考见 [configuration.md](../configuration.md)。

## 相关文档

- [commands.md](commands.md) —— 完整指令表与按键。
- [tools.md](tools.md) —— 会话级工具开关及其恢复方式。
- [configuration.md](../configuration.md) —— `config.toml`、`providers.toml`、session 头与环境变量。

[← 返回 README](../../README.md)
