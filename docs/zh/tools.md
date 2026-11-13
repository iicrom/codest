# 工具调用

[English](../tools.md) | 简体中文

codest 实现了完整的 OpenAI 工具链路。具体工具并不内建于 core：每个工具由插件声明，内置的 shell（`bash`/`pwsh`）与 `fs` 工具集本身也是普通插件（见 [plugins.md](../plugins.md) 第 3 节）。

## 工具链路

1. 插件宿主把每个启用的工具收集为一个声明好的 `ToolSpec`：名字、描述、参数 JSON Schema，以及 `read_only` 标记（纯咨询性——它为 UI 和只读 profile 过滤标记"只检查、不改动"的工具，本身不做任何强制）。
2. 这些 spec 通过请求的 `tools` 字段下发。集合为空时完全不发 `tools` 字段。
3. 模型以工具调用作答。流式 `delta.tool_calls` 分片按 call index 重组为完整调用（id、名字、原始 JSON 参数）。每个调用派发给声明该工具的插件，调用其 `run_tool(&ToolCall)`。
4. 工具返回结果文本或错误文本；两者都以 `role: "tool"` 消息、带上匹配的 `tool_call_id` 回到模型。
5. 下一轮以带有工具结果的会话快照开始。工具执行跑在 blocking pool 上，因此 `run_tool` 可以阻塞；执行期间监听中断 token，被中断的调用结果直接丢弃。

工具轮数**默认不设上限**（与 codex 一致）：模型不再调工具、请求出错或按 `Esc` 才结束回合。需要兜底时在 `config.toml` 设 `max_tool_rounds = 64`；完成该轮数后回合以 `tool round limit reached` 错误结束。

在 wire 上，声明的工具就是标准的 chat-completions `tools` 条目，其结果按 call id 作答：

```json
{"tools": [{"type": "function", "function": {"name": "read", "description": "Read a text file with line numbers.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]}
```

```json
{"role": "tool", "tool_call_id": "call_1", "content": "[exit code: 0]\n..."}
```

## 内置工具

### `bash` / `pwsh`

shell 工具是**宿主的那个**，在编译期选定：unix 上是 `bash`，Windows 上是 `pwsh`。工具名与插件 id 都跟随平台，所以 Windows 上 `/tools`、`profile.tools` 与卡片表头说的都是 `pwsh`；参数形状完全一致（`command` / `timeout`），在一端写好的调用在另一端照样能用。

- 一次一条 shell 命令——unix 为 `$SHELL -c`（`$SHELL` 未设置时回退到 `/bin/sh`），Windows 为 `pwsh -NoLogo -NoProfile -NonInteractive -Command`——工作目录为工作区。
- `timeout` 参数单位为秒，默认 **300**，上限 **600**——超限值被夹紧，而不是拒绝。
- 超时杀掉整个**进程树**：unix 向子进程的进程组发信号（`process_group(0)`），Windows 执行 `taskkill /F /T`（失败再回退到直接 kill）。后台子进程无法活过超时或继续占住管道。
- 输出合并 stdout 与 stderr（先 stdout，后 stderr），并以退出码脚注结尾（`[exit code: N]`、`[terminated by signal]` 或超时通知）。
- 按尾截断到最后 **2000 行 / 50 KB**，并附一条可操作的通知；超长单行截到 2000 字符，收集期间每条管道上限 1 MiB，失控命令不会耗尽内存。

Windows 上值得知道的差异：

- 命令被包进一段脚本，开头把 `[Console]::OutputEncoding` 钉成 UTF-8，原生命令的输出不会被控制台旧代码页弄成乱码。
- 退出码被归一：原生命令取 `$LASTEXITCODE`，失败的 cmdlet 取 `$?`（→ 1），异常被捕获后按 1 退出。裸 `pwsh -Command` 会把 cmdlet 失败报成 `0`，那会被读成成功。
- `pwsh` 是 PowerShell 7，Windows 并不自带。缺失时工具返回带安装提示的错误——不静默回退到 `powershell.exe`，那会让同一个工具在不同机器上行为不一致。

### `fs`

| 工具 | 行为 |
| --- | --- |
| `read` | 对文本文件做带行号的窗口读取，支持 `offset`（从 1 开始）与 `limit` 续读；拒绝二进制文件并指向宿主的 shell 工具（`bash`/`pwsh`）。标记为 `read_only`。 |
| `write` | 以完整内容创建或覆盖文件，按需创建父目录。 |
| `edit` | 通过 `old_string`/`new_string` 做精确字符串替换；要求唯一匹配，或使用 `replace_all`。保留 BOM 与 CRLF，未被改动的行保持原字节。 |

相对路径一律按工作目录解析。

## 会话级工具开关

`/tools` 控制本会话提供哪些已声明工具：

| 指令 | 效果 |
| --- | --- |
| `/tools` | 对已声明工具打开复选列表（`↑`/`↓` 移动，空格切换，`Enter` 把整个集合作为一个 op 应用）。 |
| `/tools <name>` | 翻转某个工具的开/关。 |
| `/tools <name> on` / `off` | 把某个工具固定为开/关。 |

`read_only` 工具在列表中带标记；未知工具名与未知方向（`on`/`off`，另有 `enable`/`disable`）会报错并列出可用工具。

- **禁用的工具会从请求的 `tools` 字段中完全移除**——省 token，也防幻觉调用。
- 模型仍然误调禁用工具时，**在执行期被拒绝**：它不会执行，模型收到 `tool … is disabled in this session (re-enable it with /tools)` 错误；名字无人声明的工具返回 `unknown tool …`。两者都像其他工具错误一样回给模型，在 wire 上以 `error: ` 前缀呈现。
- 启用集是**按会话**的，并记录在 rollout 中，因此恢复的会话沿用上次运行的工具集。
- **profile 与 `/tools` 是两个不同的切面。** profile 的 `tools` 过滤以**插件 id** 为粒度、在**装配期**（仅启动时）生效，决定哪些插件的工具被声明；`/tools` 再在**会话期**按**单个工具**微调。见 [profiles.md](../profiles.md)。

两层是叠加的：profile 写 `tools = ["fs"]` 会把 shell 工具（`bash`/`pwsh`）从该会话的世界里移除，随后 `/tools write off` 又把 `write` 对模型隐藏。被 profile 过滤掉的工具无法在当前会话恢复——那需要去掉 profile 重启。

## 工具卡片

每次调用在 transcript 中渲染为一张工具卡片：

```text
● bash(cargo build) · 4.2s   # Windows 宿主上是 `pwsh(...)`
 ⎿  Compiling codest v0.1.0
    Finished `dev` profile [unoptimized + debuginfo] target(s)
    [exit code: 0]
 … +12 lines (ctrl+o to expand)
```

- 标题是 `● <工具>(<第一个参数>) · <耗时>`，工具名使用其分类颜色。恢复的卡片没有耗时块——rollout 记录消息，不记录计时。
- `⎿` 正文挂在悬挂轨道上，显示结果的前 **3 行**。
- 其余折叠为 `… +N lines (ctrl+o to expand)`；`Ctrl+O` 展开或折叠全部工具输出（见 [commands.md](commands.md)）。折叠与展开是逐帧的视图决定，单元格本身保持无状态。
- 调用执行期间，transcript 尾部有一张带 spinner 标题与耗时行的实时卡片；工具顺序执行，因此同一时刻最多一张。

### 文件类工具的正文

文件类工具比它们的结果字符串知道得更多，所以这类卡片渲染结构化正文而不是把结果原样倒出来——与 cordis-harness-tui 的做法一致：

```text
● edit(src/tui/chat.rs) · 1.2s
 ⎿    (+2 -1) replaced 1 occurrence
    - fn body(…) { dim }
    + fn body(…) { highlighted }
    +     …

● read(src/tui/theme.rs) · 6ms
 ⎿  24 pub const TOOL_EXEC: Color = Color::Cyan;
    25 /// Read-only file tools (`read`).
       [2 lines total, showing 24-25]
```

- **`write` 与 `edit`** 渲染 diff 节选：`-` 行是这次替换掉的文本，`+` 行是写入的文本，上方是一条暗色的 `(+N -M)` 统计行，同时保留工具的一句话结论（`created`、`overwrote`、`replaced 2 occurrences`），去掉标题里已经显示的路径。`write` 没有 `-` 侧——它不知道旧内容，编造一侧只会歪曲这次改动。
- **`read`** 把带编号的窗口渲染到右对齐的暗色行号列下，`[N lines total, showing …]` 说明行缩进到代码列下方。
- 两者都按文件扩展名 **语法高亮**，`-`/`+` 色调落在标记和语法调色板未着色的部分上：新增行一眼就是绿色，而关键字与字符串仍保留自己的颜色。
- 结构化正文折叠到 **8 行**（diff 一行的信息量远高于一行命令输出，后者仍保持 3 行预算），`Ctrl+O` 与其他输出一样展开。行数在构建正文时就已计数并截断，因此一次巨大的 `write` 不会生成卡片根本显示不了的行。
- 正文是代码，因此按列宽硬折行而不是在空格处重排：折出的行能原样拼回它来自的那一行。代码在 [`src/tui/tool_body.rs`](../../src/tui/tool_body.rs)，`chat.rs` 负责卡片脊线与折叠提示。

## 离线验证整条链路

`scripts/openai_test_server.py` 是一个仅依赖 Python 标准库的 OpenAI 兼容服务，把消息里的关键词转成真实工具调用，因此无需外网即可跑通整条链路：

| 关键词 | 调用 |
| --- | --- |
| `@bash <command>`（Windows 上为 `@pwsh`） | 宿主的 shell 工具，参数 `{"command": "<command>"}` |
| `@read <path>` | `read`，参数 `{"path": "<path>"}` |
| `@write <path> <content>` | `write` |
| `@edit <path> \| <old> \| <new>` | `edit` |
| `@tool [args…]` | `--tool` 指定的工具（默认 `ping`，没有任何实现），参数来自 `--tool-arguments` |

任何不含关键词的消息都会得到列出这些关键词的 **help 回复**，所以普通发送一次即可确认服务就绪。服务还接受 `--delay`（流式 token 间隔秒数）、`--model` 与 `--dump-headers <path>`（每个请求一行含请求头的 JSON，用于断言真正发到线上的内容）。

```bash
# 终端 1
python3 scripts/openai_test_server.py --port 8080

# 终端 2
codest --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model
```

`scripts/tool_smoke.py` 端到端驱动同一条链路：真实的 shell/`read` 执行加 `/tools` 开关。它需要 unix 伪终端（`pty`），因此 Windows 上工具路径改由 `cargo test` 覆盖——那里的单元测试会真实拉起 `pwsh`，断言退出码、stderr 合并、超时杀进程树与 UTF-8 输出。

## 设计说明

- 工具是插件系统面向模型的那一半：插件用 `tools()` 声明、用 `run_tool()` 执行，core 里没有任何地方按名字认识某个工具。
- 工具错误是对话内容，而不是回合失败：参数写错、文件不存在、退出码非零都以文本回给模型，让它自行处理，链路继续。
- 插件同样通过 `system_prompt()` 提供 **skills 与 workspace instructions**；`/context` 逐段标注每个片段的出处，因此组装后的提示词可以按工具、按片段审计。

## 相关文档

- [plugins.md](../plugins.md) —— 插件契约、effects，以及内置工具插件的深入说明。
- [commands.md](commands.md) —— `/tools`、`Ctrl+O` 与其余指令面。
- [sessions.md](sessions.md) —— 工具状态与工具调用如何被记录和恢复。

[← 返回 README](../../README.md)
