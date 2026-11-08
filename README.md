# codest

`codest`（简称 `cst`）是一个从 [codex](https://github.com/openai/codex) 的 `tui` + `core` 架构中提取出来的**最小 TUI chat agent**。它只保留最基本的对话能力：输入消息、流式输出、取消当前回合、滚动历史，并带一套**编译期静态插件系统**；不包含工具调用、沙箱、审批、MCP、会话持久化等 codex 的完整能力。

项目目录仍为 `../cst`（即 `codex` 的兄弟目录），与 codex 仓库完全独立。

## 安装

```bash
cd ../cst
cargo install --path .
```

安装后会得到两个等价的命令：

```bash
codest   # 主命令
cst      # 简称别名，参数与 codest 完全相同
```

## 快速开始

```bash
cd ../cst

# 终端 1：启动本地 OpenAI 兼容测试服务（回显最后一条用户消息）
python3 scripts/openai_test_server.py --port 8080

# 终端 2：把 codest 指向本地服务；测试服务不校验 key（下面两条任选其一）
cargo run -- --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model
cargo run -- --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model --tui-mode inline

cargo test                    # 单元测试离线运行，使用进程内假模型，不访问网络
python3 scripts/pty_smoke.py  # 伪终端冒烟测试：自动拉起测试服务，fullscreen / inline 都会跑
```

连接一个 OpenAI 兼容的流式接口：

```bash
export CST_API_KEY=sk-...
export CST_BASE_URL=https://api.openai.com/v1   # 可选
export CST_MODEL=gpt-4o-mini                   # 可选
cargo run
```

也可以显式传参：

```bash
cargo run -- --api-key sk-... --base-url https://api.openai.com/v1 --model gpt-4o-mini
```

恢复上次在这个工作区的对话：

```bash
cargo run -- --continue    # 直接继续最近一次保存的会话
cargo run -- --resume      # 打开会话选择菜单（见"多会话"）
```

> `CST_API_KEY` / `CST_BASE_URL` / `CST_MODEL` / `CST_TUI_MODE` 中的 `CST` 就是 codest 的简写，保持不变。
>
> 没有 API key 时 codest 会直接报错并提示如何启动本地测试服务，不再回退到任何内嵌 mock。

### 本地测试服务

`scripts/openai_test_server.py` 是只依赖 Python 标准库的简易 OpenAI Chat Completions 服务端，用于在没有 key、没有外网的情况下端到端验证 TUI 和 SSE 客户端：

- `POST /chat/completions` 支持 `"stream": true` 的 SSE 增量输出，也支持普通 JSON 响应；
- `GET /health` 返回 `{"status":"ok"}`；
- 不校验 API key，回复内容为最后一条用户消息，便于确认流式渲染；
- `--delay` 控制 token 间隔，`--port 0` 使用随机端口并打印实际地址。

```bash
python3 scripts/openai_test_server.py --port 0
# codest test server listening on http://127.0.0.1:xxxxx/v1
```

> 按需求，本项目**没有做真实外网测试**。`cargo test` 是纯离线单元测试；`scripts/pty_smoke.py` 只访问本机启动的测试服务。

## TUI 模式（fullscreen / inline）

和 pi 的 `tuiMode`（`fullscreen` / `regular`）以及 cht 的 `fullscreen` 设置一样，codest 支持两种显示模式，默认是 **fullscreen**：

| 模式 | 行为 |
| --- | --- |
| `fullscreen`（默认） | 进入 alternate screen，由应用持有整屏；transcript 用 `PgUp`/`PgDn` 或鼠标滚轮滚动 |
| `inline` | 不进入 alternate screen；完成的 transcript 直接写进终端自己的 scrollback，底部只保留输入框和状态行；`PgUp`/`PgDn` 交还终端 |

切换方式：

```bash
codest --tui-mode inline     # regular 是 inline 的别名
cst --tui-mode inline        # cst 是等价的命令别名
CST_TUI_MODE=inline codest   # 也可以用环境变量
```

`fullscreen` 会开启鼠标上报（滚轮滚动 transcript；`Alt`+滚轮一次 15 行）。需要终端原生选区时按住 `Shift` 拖拽。`inline` 不开启鼠标上报，保留终端原生 scrollback 和选区。

## 操作

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送消息；指令候选可见时执行选中的指令 |
| `Tab` | 指令候选可见时补全选中的指令 |
| `Ctrl+J` / `Alt+Enter` / `Shift+Enter` | 插入换行 |
| `↑` / `↓` | 指令候选可见时选择候选；多行时移动光标；单行时浏览历史输入 |
| `Ctrl+P` / `Ctrl+N` | 浏览历史输入 |
| `←` / `→` / `Home` / `End` | 光标移动 |
| `Backspace` / `Delete` | 删除字符 |
| `Ctrl+W` / `Alt+Backspace` | 删除前一个词 |
| `Ctrl+U` / `Ctrl+K` | 删除到行首 / 行尾 |
| 滚轮（fullscreen） | 滚动聊天记录（每格 3 行，`Alt`+滚轮 15 行） |
| `PageUp` / `PageDown`（fullscreen） | 滚动聊天记录；inline 下交给终端 scrollback |
| `Esc` | 关闭指令候选；生成中则中断正在生成的回复 |
| `Ctrl+C` | 生成中：中断；有空输入时第一下清空；输入为空时连按两次退出 |
| `Ctrl+D` | 输入为空时退出 |

## 指令

在输入框里输入以 `/` 开头的文本即可执行指令；指令由静态插件提供，在本地处理，不会发送给模型：

| 指令 | 别名 | 行为 |
| --- | --- | --- |
| `/help` | `/h`、`/?` | 显示指令列表（包含所有插件贡献的指令） |
| `/clear` | — | 清空会话上下文和屏幕上的 transcript |
| `/quit` | `/exit` | 退出 codest |
| `/status` | — | 显示 backend、模式、transcript 大小和已加载插件 |
| `/new` | — | 归档当前对话，开启一个新的空会话 |
| `/rename <name>` | — | 重命名当前会话；默认名取该会话第一句用户输入 |
| `/sessions` | `/resume` | 打开会话列表菜单，切换并继续以前的会话 |

输入 `/` 就会在输入框上方弹出候选列表，继续输入会按指令名或别名前缀实时过滤（不区分大小写）：

- 指令的别名和主名一样是独立候选：别名行标注 `(alias of /主名)`，`Tab`/`Enter` 直接补全为别名本身；
- `↑` / `↓` 在候选之间循环选择，`Tab` 补全选中的指令；
- `Enter` 先补全再直接执行选中的指令，`Esc` 关闭候选但不影响已输入的内容；
- 候选只在输入还是“指令名”阶段出现；一旦输入空格或换行，就按普通文本处理。

只有整条输入的第一个非空白字符是 `/` 时才会被当作指令，其他文本照常发给模型。未知指令、多余参数或缺少参数会显示错误提示，不会发给模型。

## 多会话

codest 支持多轮独立对话：会话按创建顺序编号（从 1 开始）且不会被删除，模型上下文和屏幕 transcript 都按会话各自记录。

| 指令 | 行为 |
| --- | --- |
| `/new` | 归档当前会话，开启一个新的空会话并切换过去 |
| `/rename <name>` | 重命名当前会话，用于会话列表展示 |
| `/sessions`（`/resume`） | 打开会话列表菜单：`↑`/`↓` 选择、`Enter` 切换并继续、`Esc` 关闭 |

- 会话默认以该会话的第一句用户输入命名（参考 dsh），`/rename` 可以随时改名。
- 同一时刻只有一个会话在生成；生成中的输入会被忽略，`/new` 与 `/sessions` 都要在回合结束后使用（必要时先 `Esc` 中断）。
- 切换会恢复该会话的模型上下文，后续对话继续在原历史上进行；`/clear` 只影响当前会话。
- 会话列表每行显示会话名、记录条数，当前会话标有 `current`。
- `/new`、`/rename` 与 `/sessions` 是本体常驻命令，不属于插件系统。

### 会话持久化（rollout）

每次对话都按 codex 的 rollout 方案蒸馏后落盘：工作区级隔离，跨次启动可恢复。

- 存储在 `~/.codest/sessions/<工作区哈希>/rollout-<id>.jsonl`（根目录可用 `CODEST_HOME` 覆盖，镜像 codex 的 `CODEX_HOME`；工作区哈希 = 启动目录的 FNV-1a）；
- 文件是 append-only JSONL：首行是会话元数据（id、创建时间、工作区、系统提示词），之后用户消息、完成的助手消息、`/clear`、`/rename` 各记一行——回放这些行即可精确重建当时的对话；
- 还没有输入过的空会话不写 rollout 文件：文件在该会话收到第一条用户消息时才创建，随手开一下 `/new` 不会在磁盘上留垃圾；
- 写入是尽力而为：落盘失败只影响持久化，不影响对话（会话留在内存）；
- 每次启动都会加载本工作区已保存的会话，`/sessions` 在任何启动方式下都是同一份列表。默认激活一个新会话；也可以显式指定恢复方式：

```bash
codest --continue        # 直接激活最近一次保存的会话（-c 等价）
codest --resume          # 同 --continue，并在启动时打开会话选择菜单
```

## 插件（编译期静态）

codest 的指令和生命周期扩展都来自**编译期链接**的插件，不做运行时加载：

- 插件是普通的 Rust 模块，注册表是 `src/plugins/builtin/mod.rs` 里的一个 `static` 切片；注册顺序就是钩子执行顺序和菜单顺序。
- 插件可以贡献 slash 命令、系统提示词片段，以及 `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` 钩子。
- 插件不直接操作终端或会话，只返回 `PluginEffect`（通知、错误、清屏、退出、设置输入框、提交新回合），由 TUI 统一应用；因此插件代码不依赖 ratatui，也可以被其他前端复用。
- 重复插件 id、重复命令名/别名、非法命令名会在启动时被拒绝。

内置的 `/help`、`/clear`、`/quit`、`/status` 本身就是插件，可以作为示例。完整契约、执行顺序、错误处理和“新增插件”的完整代码见 [`docs/plugins.md`](docs/plugins.md)。

新增插件的最小步骤：

1. 在 `src/plugins/builtin/` 下新建模块，声明 `static` 插件值和 `CommandSpec`；
2. 实现 `Plugin`（至少 `id`，按需实现 `commands` / `run_command` / 钩子）；
3. 在 `src/plugins/builtin/mod.rs` 的 `BUILTIN_PLUGINS` 里加一项；
4. 运行 `cargo test`。

## 从 codex 提取了什么

| codex 模块 | codest 对应模块 | 说明 |
| --- | --- | --- |
| `codex-rs/protocol`（SQ/EQ） | `src/core/protocol.rs` | `Op` / `Event` 消息总线，UI 不直接调用模型 |
| `codex-rs/core` 的会话循环 | `src/core/session.rs` | worker 持有全部会话历史与取消 token，产出事件 |
| codex 的多会话管理 | `src/core/sessions.rs` + `src/tui/session.rs` | 会话命令 spec 与每会话视图状态（`/new`、`/sessions`） |
| codex 的 rollout 持久化 | `src/core/rollout.rs` | 会话 JSONL 落盘与回放（`~/.codest/sessions/<工作区哈希>/`） |
| `codex-rs/core/src/client.rs` | `src/core/model.rs` | 流式模型客户端抽象 |
| codex 的 Responses client | `src/core/openai_compat.rs` | 最小 OpenAI 兼容 Chat Completions + SSE 解析 |
| codex 的 fake/OSS provider | `scripts/openai_test_server.py` | 本地 OpenAI 兼容测试服务（crate 内不再内置 mock 后端） |
| `codex-rs/tui/src/app.rs` | `src/tui/app.rs` | 终端事件循环、渲染、输入处理 |
| `codex-rs/tui/src/bottom_pane/textarea.rs` | `src/tui/composer.rs` | 多行输入、光标、换行映射 |
| `codex-rs/tui/src/history_cell*` | `src/tui/chat.rs` | 用户 / 助手 / 错误 / 提示 / 中断单元格 |
| codex 的 wrapping | `src/tui/wrap.rs` | Unicode 宽度感知换行与光标定位 |

codex 的 TUI 超过 10 万行、`core` 也有大量工具/沙箱/会话基础设施，且与 app-server 深度耦合，无法原样裁剪。这里提取的是**可独立编译的架构骨架**，而不是逐文件复制。

## 目录

```
codest/
├── Cargo.toml
├── README.md
├── docs/
│   └── plugins.md           # 静态插件系统设计
├── scripts/
│   ├── openai_test_server.py # 本地 OpenAI 兼容测试服务
│   └── pty_smoke.py          # 离线端到端冒烟测试（pty）
└── src/
    ├── main.rs              # codest 主入口
    ├── bin/cst.rs           # cst 别名入口
    ├── cli.rs               # CLI 参数 + endpoint 配置
    ├── lib.rs
    ├── core/
    │   ├── protocol.rs      # Message / Op / Event
    │   ├── commands.rs      # 命令表 / 解析（指令由插件提供）
    │   ├── sessions.rs      # 内置 /new /rename /sessions 命令 spec（多会话）
    │   ├── conversation.rs  # 线性会话历史
    │   ├── model.rs         # ModelClient trait + 错误类型
    │   ├── openai_compat.rs # HTTP + SSE 流式客户端
    │   ├── rollout.rs       # 会话持久化（JSONL rollout 落盘 / 回放）
    │   ├── test_model.rs    # 仅 cargo test 编译的进程内假模型
    │   └── session.rs       # agent worker（持有全部会话）+ AgentSession
    ├── plugins/
    │   ├── mod.rs           # Plugin trait / PluginHost / effects
    │   └── builtin/
    │       ├── mod.rs       # 静态注册表
    │       ├── help.rs      # /help
    │       ├── clear.rs     # /clear
    │       ├── quit.rs      # /quit
    │       └── status.rs    # /status
    └── tui/
        ├── app.rs           # 事件循环 / fullscreen & inline 渲染
        ├── session.rs       # 每会话视图状态（SessionView）
        ├── chat.rs          # 历史单元格
        ├── composer.rs      # 输入编辑器
        ├── theme.rs         # 颜色
        └── wrap.rs          # 换行与光标映射
```

## 已知边界

- 只支持纯文本对话，没有工具调用、文件读写、审批、MCP、图片。
- 插件是编译期静态链接的：新增/修改插件需要改代码并重新编译，不支持运行时加载 `.so` 或脚本插件；插件与主程序同权限，没有沙箱隔离。
- 插件钩子在 TUI 线程同步执行，不能做阻塞 I/O。
- 会话按 rollout 保存在 `~/.codest/sessions/<工作区哈希>/`（`CODEST_HOME` 可覆盖），跨次启动可用 `--continue` / `--resume` 恢复；没有并发写锁（同时开多个 codest 写同一工作区没有保护）、没有归档/删除/搜索，也没有跨工作区的会话迁移。
- 没有网络重试、代理、认证刷新；HTTP 客户端只实现一次请求 + SSE 增量解析。
- 没有内嵌离线模型：必须配置 API key，或先启动 `scripts/openai_test_server.py` 并把 `--base-url` 指过去。
- `fullscreen` 只有滚轮滚动，应用内鼠标选区/复制尚未实现；选区用终端原生能力（通常按住 `Shift` 拖拽）。
- `inline` 下已经写进 scrollback 的行不会随窗口宽度重新换行。
- 测试全部离线：`cargo test` 不会发起任何网络请求；`scripts/pty_smoke.py` 只访问本机启动的测试服务。

## 后续可扩展点

1. 在 `Op` 中加入工具调用与审批事件，向 codex 的 SQ/EQ 靠拢，并让插件可以注册工具。
2. 为 `Conversation` 增加 token 预算与压缩。
3. 会话持久化（rollout JSONL）与 resume。
4. Markdown / 代码高亮渲染（codex 使用 `pulldown-cmark` + `syntect`）。
5. `fullscreen` 下的应用内鼠标拖选与 OSC 52 复制（对齐 pi/cht）。
6. 用 `inventory`/`linkme` 做分布式静态注册，保持 `Plugin` 契约不变。
