# 指令与按键

[English](../commands.md) | 简体中文

slash 指令是 TUI 的本地控制面：它们在本地解析并在前端执行，**永远不会发送给模型**。输入 `/` 弹出实时过滤的候选列表；`↑`/`↓` 选择、`Tab` 补全、`Enter` 补全并执行、`Esc` 关闭。

## Slash 指令

| 指令 | 别名 | 行为 |
| --- | --- | --- |
| `/help` | `/h`、`/?` | 显示指令列表（含插件贡献的指令）。 |
| `/clear` | — | 清空会话上下文与屏幕 transcript。 |
| `/quit` | `/exit` | 退出 codest。 |
| `/status` | — | 显示 backend、模式、transcript 大小与已加载插件。 |
| `/context` | — | 显示模型实际收到的内容：工具及启用状态、组装后的系统提示词（逐段标注出处）、请求配置与 token 用量。会话中可浏览、删除、编辑上下文条目。 |
| `/new` | — | 归档当前对话，开启新的空会话。 |
| `/rename <name>` | — | 重命名当前会话（默认名取第一句用户输入）。 |
| `/sessions` | `/resume` | 打开会话列表，切换并继续以前的会话。 |
| `/continue` | — | 不追加用户消息、直接续跑当前会话——崩溃/中断后的恢复入口。 |
| `/model` | — | 打开模型菜单，切换 provider/model（`r` 重新发现）。 |
| `/provider` | — | 打开 provider 向导：新增 OpenAI 兼容端点、登录插件 provider、查看已配置列表。 |
| `/profile [name]` | — | 显示可用 profile 列表，或切换到某个 profile（工具与提示词只在启动时生效）。 |
| `/effort [level]` | — | 查看或设置推理等级（默认 `minimal`/`low`/`medium`/`high`，`off` 清除），以 `reasoning_effort` 下发。 |
| `/thinking` | — | 切换 thinking 卡片展开/折叠（等同 `Ctrl+T`）。 |
| `/tools [name] [on\|off]` | — | 查看或设置本会话启用的工具；禁用的工具从请求中移除。 |
| `/settings [section]` | — | 打开设置界面：宿主与插件选项，保存进 `config.toml`。参数可直接聚焦某个分区。 |

与会话相关的指令、回合边界规则以及切换后恢复什么，见 [sessions.md](sessions.md)；`/tools` 与工具链路见 [tools.md](tools.md)。

## 解析

一次提交在发送时被分类一次：

- 忽略首尾空白；`/` 之前的行首空白允许存在（`  /help` 可用）。
- 指令名延伸到第一个空白为止，匹配**大小写不敏感**，主名与别名同等对待。
- 参数是那之后的全部内容（去除空白）。每条指令声明自己的参数策略：

| 策略 | 指令 | 多出的参数 |
| --- | --- | --- |
| 无 | `/help`、`/clear`、`/quit`、`/status`、`/context`、`/new`、`/sessions`、`/continue`、`/model`、`/provider`、`/thinking` | 报错并指明该指令 |
| 可选 | `/profile`、`/effort`、`/tools`、`/settings` | 属于指令本身（profile 名、等级、工具名、分区名） |
| 必需 | `/rename` | — 参数为空时报错 |

- 只有 `/`（没有名字）与未知名字都是打印到 transcript 的错误，不会发给模型。
- 即使没有可用路由，指令也能工作：它们不需要模型。

## 指令从哪来

出厂指令表合并了两组来源，解析、候选菜单与 `/help` 读的是同一张表：

| 分组 | 指令 | 归属 |
| --- | --- | --- |
| 内置插件 | `/help`、`/clear`、`/quit`、`/status`、`/continue` | `src/plugins/builtin/` —— 同时作为插件示例。 |
| 前端指令 | `/new`、`/rename`、`/sessions`、`/model`、`/provider`、`/profile`、`/effort`、`/thinking`、`/tools`、`/context`、`/settings` | core/TUI，在插件宿主之前派发。 |

插件贡献的指令进入同一张表，因此自动获得候选菜单与 `/help`；名字与别名在启动时校验且必须全局唯一。见 [plugins.md](../plugins.md)。

## 候选列表

- 一个候选就是一个**名字**：别名是独立候选，紧跟在所属指令之后，所以 `/h` 与 `/help` 是两行。
- 过滤是**大小写不敏感的前缀匹配**，主名与任意别名都参与：`/RE` 仍能匹配 `/rename` 与 `/resume`，输入 `/?` 通过 `?` 别名匹配 `/help`。
- 只有指令**名字**还在输入时才出现候选：第一个空白即结束候选窗口，所以 `/tools ba` 不显示菜单。
- 第一个非空白字符必须是 `/`；它之前的行首空白被忽略，其余情况都是普通对话输入。
- 空查询（只输入 `/`）按注册顺序列出全部指令。
- 候选列表最多显示 **6 行**；可见窗口跟随选中项，`↑`/`↓` 在两端循环。
- 可见的候选列表是权威的：即使只输入了前缀，`Enter` 也执行高亮的那条指令。`Esc` 关闭列表并保留文本；任何编辑都会重新打开列表并把选中项重置到第一个候选。
- 生成中、或某个选择器/向导（`/sessions`、`/model`、`/effort`、`/tools`、`/provider`）打开时，候选列表保持关闭。

```text
/co
▸ /context                 before the first message: show the prompt report; in a session: browse, delete, and edit the context
  /continue                continue the current session without a new message (resume an interrupted turn)
```

每行是 `▸ /名字`（固定宽度名字列）、指令摘要，以及——若是别名——`(alias of /名字)`；主名则列出自己的别名。

## 按键

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送；候选可见时执行选中的指令。 |
| `Tab` | 补全选中的指令。 |
| `Ctrl+J` / `Alt+Enter` / `Shift+Enter` | 插入换行。 |
| `↑` / `↓` | 选择候选；多行移动光标；单行浏览输入历史。 |
| `Ctrl+P` / `Ctrl+N` | 浏览输入历史。 |
| `←` / `→` / `Home` / `End` | 光标移动。 |
| `Backspace` / `Delete` | 删除字符。 |
| `Ctrl+W` / `Alt+Backspace` | 删除前一个词。 |
| `Ctrl+U` / `Ctrl+K` | 删除到行首 / 行尾。 |
| 滚轮（fullscreen） | 滚动 transcript（每格 3 行，`Alt`+滚轮 15 行）。 |
| `PageUp` / `PageDown` | 滚动 transcript（fullscreen）；inline 下交给终端。 |
| `Esc` | 关闭候选；生成中则中断当前回合。 |
| `Ctrl+T` | 展开/折叠 thinking 卡片。 |
| `Ctrl+O` | 展开/折叠工具输出（折叠时显示前 3 行预览）。 |
| `Ctrl+C` | 生成中中断；清空输入；空输入时连按两次退出。 |
| `Ctrl+D` | 空输入时退出。 |

### 说明

- **fullscreen 与 inline 的差异。** `PageUp`/`PageDown` 只在 `fullscreen` 归应用处理——它持有 alternate screen。`inline` 下 scrollback 归终端所有，因此这两个键直接交给终端。
- **`Ctrl+T` 就是 `/thinking`。** 两者都切换 thinking 卡片定型后是否展开；卡片本身始终被记录。
- **`Ctrl+O` 折叠工具输出。** 卡片正文显示前 3 行，其余折叠为 `… +N lines (ctrl+o to expand)`；`Ctrl+O` 作用于 transcript 中全部工具卡片。
- **`Ctrl+C` 有三种状态。** 生成中中断当前回合；输入框有文本时清空输入；空输入时必须快速连按两次才退出。
- **`Ctrl+D`** 只在空输入时退出——有文本时不做任何事。

## 相关文档

- [sessions.md](sessions.md) —— 会话指令、回合边界规则、持久化与恢复。
- [tools.md](tools.md) —— `/tools`、工具链路与工具卡片。
- [configuration.md](../configuration.md) —— `config.toml`、`providers.toml`、`profiles.toml` 与 session 头。
- [tui.md](tui.md) —— fullscreen 与 inline 渲染、Markdown 与 thinking 卡片。

[← 返回 README](../../README.md)
