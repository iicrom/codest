# 终端界面

[English](../tui.md) | 简体中文

codest 有两种占用终端的方式。`fullscreen` 把整个屏幕交给应用，transcript 在应用自己滚动的视口里绘制；`inline` 让终端继续掌权，把已定型的输出追加到终端原生 scrollback。其余部分——Markdown 渲染、thinking 卡片、工具卡片、输入框、指令菜单——两种模式行为一致，因为二者渲染的是同一批 transcript 单元格，区别只在于这些单元格落在哪里。本页讲显示侧；按键总表见 [commands.md](../commands.md)。

## TUI 模式

| 模式 | 行为 |
| --- | --- |
| `fullscreen`（默认） | 进入 alternate screen，应用持有整屏；transcript 用 `PgUp`/`PgDn` 或滚轮滚动。开启鼠标上报，滚轮每格 3 行（`Alt`+滚轮 15 行）；按住 `Shift` 拖拽使用终端原生选区。 |
| `inline` | 不进 alternate screen；已定型的 transcript 行写入终端自身 scrollback；底部只保留输入框与状态行。不开启鼠标上报，选区与复制保持终端原生行为。 |

两种模式的差别只有一条契约：`fullscreen` 下应用重绘视口，随时可以改任何内容；`inline` 下已经进入 scrollback 的行脱离了应用的控制，无法再修改。正因如此，`inline` 只写入渲染器认为已经定型的行（见[流式提交规则](#流式提交规则)）。

```bash
codest --tui-mode inline     # regular 是 inline 的别名
CST_TUI_MODE=inline codest
```

`--tui-mode` 与 `CST_TUI_MODE` 接受 `fullscreen` 和 `inline`；CLI 另外接受 `regular` 作为 `inline` 的拼写，与其他终端 agent 的用词保持一致。

### 滚动

`fullscreen` 下 transcript 位于应用自管的滚动视口：滚轮每格 3 行，`Alt`+滚轮 15 行，`PgUp`/`PgDn` 每次 5 行。应用重绘视口，所以 transcript 可以长于屏幕、上下都能浏览。

`inline` 下应用不做任何滚动：`PgUp`/`PgDn` 直接交给终端，由终端滚动自己的 scrollback。回合进行中，inline 面板在底部最多保留四行仍在流式生长的尾部，高度固定——因此逐行增长的块不会把自己已经画好的开头滚进 scrollback、事后又重复输出一次。块定型后，这些行才真正晋升进 scrollback。

### 屏幕布局

两种模式都从上到下排同样的四段：transcript、菜单带、输入框、footer。

- **Transcript。** 用户回合带 `› ` 前缀；助手回答首行是 `• `，后续行缩进在正文下；错误用 `! `；通知是暗色、无前缀；被中断的回合留下 `■ interrupted` 标记。thinking 卡片、工具卡片以及 `/context` 这类插件报告都是这份列表里的单元格。transcript 按会话保存：切换会话即切换渲染的视图，后台会话重新激活时补上差距。
- **菜单带。** slash 指令候选弹窗，或打开中的某个选择器（`/sessions`、`/model`、`/effort`、`/tools`）。没有内容需要它时收缩为零行。`/provider` 向导也渲染在这里，是一组选项行而不是覆盖层。整帧界面（`/context`、`/settings`）则整体替换布局，而不是占用这条带。
- **整帧界面。** `/settings` 与 `/context` 共用同一套框架（`src/tui/screen.rs`）：带 `n/m` 计数器的头部、可滚动的列表、可选的固定面板（`/context` 的检视区）、分隔线、提示行和帮助栏。两者都是 `↑`/`↓` 在列表中移动并在两端循环——第一行按 `↑` 跳到末行，末行按 `↓` 跳回首行——滚轮只滚动列表、不移动选择，所以浏览不会改变按键作用的对象；下一次按键会把视口重新锚定到选择上。
- **输入框。** 多行编辑器，只有一条上边框，`› ` 提示符占自己的两列 gutter，最多显示八行；更长的输入在内部滚动。开启了 bracketed paste，多行粘贴作为文本落入，而不是一串按键。
- **Footer。** 左侧是 `codest · <provider/model> · effort <等级> · <上下文>`，然后一个 `│` 和状态词——`ready`，或回合进行中的转轮加 `working · Esc interrupt`。上下文读数始终存在：两值都已知时是 `used/window (n%)`，只知道窗口时是 `—/window`，只知道用量时是 `used tok`，两者都未知时是 `—`。它随每个流式回合报出的 usage 更新，长工具轮次期间也不会停在旧值，回合结束后由完成的那次 usage 定稿。右侧是当前情境的提示行；`thinking expanded` 这类短暂反馈会临时占位两秒左右。窄终端上提示按整条裁剪——在 `·` 边界切开而不是从词中间截断，所以 `Enter send · / commands` 会变成 `Enter send`，而不是 `Enter send · / comma`。

终端 resize 在下一帧重绘；`fullscreen` 下全部按新宽度重新换行，`inline` 下只有尚未进入 scrollback 的行会重排（见[已知边界](development.md#已知边界)）。

## Markdown 渲染

助手回答按 Markdown 渲染。用户输入、通知、错误与思考内容仍是纯文本——只有模型的回答经过渲染器。支持的语法：

- 标题，分级体现在样式上；
- 列表，支持嵌套与悬挂缩进；
- 引用，用 `▎` 竖条绘制；
- 表格，按内容对齐，超出面板宽度时压缩；
- 任务复选框；
- 链接，文字着色、地址以暗色括注；
- 图片，显示 alt 文本；
- 行内样式（粗体、斜体、行内代码、删除线）；
- 围栏与缩进代码块，带语言标签与语法高亮。

在 transcript 里，回答的每一行都缩进在助手缩进线之下，Markdown 结构——标题、圆点、引用竖条、表格线、围栏标签——因此保留自己的标记。换行在渲染之后进行，且感知 Unicode 宽度，宽字符或组合 emoji 不会让后续行错位。

流水线是单趟的：`pulldown-cmark` 产出事件，处理器把每个事件变成带样式的 ratatui span，最后再换行、加前缀。CommonMark 之外只开启了表格、删除线与任务列表。表格实际上排了两遍——先量出每列宽度，超宽时再压缩最宽的那列——所以宽表格是整体压缩，而不是在单元格中间折行。图片会带 `🖼 ` 标记再跟 alt 文本；裸 autolink 不在括号里重复地址，因为标签本身就是 URL。

### 流式提交规则

回复流式输出时，只有**已定型**的顶层块写入 scrollback，正在生长的最后一个块在底部面板按每个增量重绘。所谓定型，是指解析器再也无法改动它已经产出的行：未闭合的 `**`、半张表格、没收尾的代码围栏都会改变已经打印的行，而 `inline` 模式下这些行收不回来。`inline` 下 scrollback 归终端所有，这条提交规则正是长回答中历史保持稳定的原因。

### 代码高亮

高亮走 `TokenKind` 接口（[`src/tui/highlight.rs`](../../src/tui/highlight.rs)），背后的引擎可替换，渲染器从不接触引擎类型。当前引擎是 `syntect`，启用纯 Rust 的 `regex-fancy` 特性：不需要 oniguruma，也不需要 C 工具链。语法集来自 `two-face`，覆盖了 syntect 自带语法集缺失的语言（TypeScript、TOML、Dockerfile 等）。

接口刻意做小：输入一块源码行，输出每行的 `TokenKind` 片段列表，再由一个独立的调色函数把每种 kind 映射到颜色。接缝之上没有任何地方知道 syntect 存在，换引擎只需产出同一批 kind：plain、keyword、type、function、string、number、comment、constant、attribute。

未知语言或超过 128 KiB 的块退化为纯文本，但保留语言标签。同一条兜底路径也拦下异常长的单行，超大的粘贴因此仍然可读，而不会卡住正在渲染的那一帧。高亮过的块按语言与内容缓存，并设有条目数与字节数上限——围栏流式进入时，每个增量都是活动块的一个新变体，正是这两个上限让内存保持平稳。

## Thinking 卡片

模型思考渲染为可折叠卡片：

```text
⚓ thinking · 4.2s (ctrl+t to expand)
  │ …最新一行推理，超宽时从开头截断…
```

展开时，卡片在轨道下显示整段推理：

```text
⚓ thinking · 4.2s
  │ dim italic reasoning…
  │ …同一根轨道上继续挂着的正文…
```

头部是 `⚓ thinking` 加一个用时标签，使用 thinking 颜色（紫色）；正文是挂在 `│` 轨道下的斜体文本。`Ctrl+T` 或 `/thinking` 切换已定型卡片是否展开正文，折叠的卡片会标明自己的展开键。

折叠状态仍保留 **一行上下文**：最后一行非空的推理挂在轨道下；当它宽于卡片时从 **开头** 截断（前导 `…`）。模型推理时，新 token 落在一行的末尾，因此裁掉头部才能让预览跟着流走，而不是卡在旧词上——这正是参考 TUI 的 ticker 规则。活动版本同理，折叠的卡片因此不只回答“它在思考”，还回答“此刻在思考什么”。纯空白的推理不产生预览，卡片只留头部。

推理流式输出期间，卡片是活动版本：转轮替换锚形图标，用时标签等流结束才出现，块光标骑在预览行上（展开时骑在正文最后一行）。

思考文本默认读取 `reasoning_content`，provider 实例可用 `wire.thinking_fields` 改名。卡片是显示元数据：恢复会话时回放，让 transcript 看上去与当时一致，但思考内容永远不会重新进入模型上下文——模型不会把自己的推理当作会话内容再读一遍。rollout 记录每个推理块及其实际耗时（即头部标签的来源），并把它挂在其先行的那条助手消息上，回放时卡片因此落回正确的位置。

折叠状态按帧决定，不按卡片持久化：`Ctrl+T` 翻一个全局开关，所有卡片按它渲染。`inline` 下已经写进 scrollback 的卡片保留当时的展开状态——scrollback 无法为后来的切换重写。

## 工具卡片

每次工具调用渲染为一张卡片：

```text
● bash(cargo build) · 4.2s   # Windows 宿主上是 `pwsh(...)`
 ┌  Compiling codest
 │  Finished `dev` profile
 └  … +3 lines (ctrl+o to expand)
```

头部是 `● name(args) · duration`，工具名用所属类别的颜色，正文外框同色。正文用同一套制表符画成一个框——`┌` 起头、`│` 承载中间行、`└` 收尾——折角与竖线落在同一列，因此多深的正文都连成一块。正文预览前 3 行；`Ctrl+O` 展开/折叠完整输出。调用仍在执行时，转轮占据状态图标位，耗时行作为收尾的框挂在标题下方。失败时 `●` 换成 `✗`，错误信息以错误色代替输出显示。参数摘要被折叠时（典型是多行命令），展开会在输出上方恢复完整参数。

文件类工具渲染 **结构化正文** 而不是结果文本，因为它们比结果字符串知道得更多——`write`/`edit` 卡片显示高亮的 `-`/`+` 节选并配一条 `(+N -M)` 统计行，`read` 卡片把带编号的窗口放在暗色行号列下：

```text
● edit(src/tui/chat.rs) · 1.2s
 ┌    (+2 -1) replaced 1 occurrence
 │  - fn body(…) { dim }
 └  + fn body(…) { highlighted }
```

这类正文折叠到 8 行而不是 3 行，按文件扩展名语法高亮，并像代码一样按列宽硬折行。细节见 [tools.md](tools.md#文件类工具的正文)。

工具活动只是显示内容——它从不作为 agent 文本出现。会话里保存的是线协议消息（助手的 `tool_calls` 与每次工具的结果）；卡片是前端自己的账务。恢复会话时卡片由 rollout 重建，只有本轮实测过的调用才带用时标签。

## 实现地图

| 关注点 | 文件 |
| --- | --- |
| 终端装配、事件循环、两条渲染路径、菜单、footer | [`../src/tui/app.rs`](../../src/tui/app.rs) |
| transcript 单元格（用户、助手、错误、通知、中断、两类卡片、报告） | [`../src/tui/chat.rs`](../../src/tui/chat.rs) |
| 结构化工具正文：`write`/`edit` 的 diff 节选、`read` 的行号列 | [`../src/tui/tool_body.rs`](../../src/tui/tool_body.rs) |
| 每会话视图状态：单元格、滚动、折叠镜像、inline 刷出记账 | [`../src/tui/session.rs`](../../src/tui/session.rs) |
| Markdown → ratatui 行，含表格、围栏、链接 | [`../src/tui/markdown.rs`](../../src/tui/markdown.rs) |
| `TokenKind` 之后的代码分词 | [`../src/tui/highlight.rs`](../../src/tui/highlight.rs) |
| 换行与光标到行的映射 | [`../src/tui/wrap.rs`](../../src/tui/wrap.rs) |
| 输入框编辑 | [`../src/tui/composer.rs`](../../src/tui/composer.rs) |
| 整帧的 `/context` 查看器 | [`../src/tui/context_view.rs`](../../src/tui/context_view.rs) |
| `/provider` 向导状态机 | [`../src/tui/provider_wizard.rs`](../../src/tui/provider_wizard.rs) |
| 整帧的 `/settings` 界面（字段 → 行、写入） | [`../src/tui/settings_view.rs`](../../src/tui/settings_view.rs) |
| 整帧界面框架（头部、分区条、分隔线、提示、帮助栏、滚动） | [`../src/tui/screen.rs`](../../src/tui/screen.rs) |
| 卡片边框、指针 gutter、行与值列 | [`../src/tui/cards.rs`](../../src/tui/cards.rs) |
| 列表界面共用的一行编辑器 | [`../src/tui/inline_edit.rs`](../../src/tui/inline_edit.rs) |
| 调色板 | [`../src/tui/theme.rs`](../../src/tui/theme.rs) |

启动时会往当前会话 transcript 的最前面放三条 notice：一行 banner、一行 `backend: <provider/model> · mode: <模式>`，以及一行按键提示（两种模式内容不同）。恢复会话时，同样三条会接在已恢复的 transcript 之上，后面再跟一行恢复会话数；普通启动且存在已保存会话时，会有一行提示指向 `/sessions`。它们就是普通的通知单元格——`/clear` 会和其余 transcript 一起清掉。

## 相关文档

- [commands.md](../commands.md) —— 全部 slash 指令与按键，含 `Ctrl+T` 与 `Ctrl+O`。
- [configuration.md](../configuration.md) —— `wire.thinking_fields`、provider 实例以及其余文件参考。
- [tools.md](../tools.md) —— 这些卡片所渲染的 shell（`bash`/`pwsh`）与 `fs` 工具。
- [../src/tui/app.rs](../../src/tui/app.rs) —— 事件循环与两条渲染路径。
- [../src/tui/chat.rs](../../src/tui/chat.rs) —— transcript 单元格，含两类卡片。

[← 返回 README](../../README.md)
