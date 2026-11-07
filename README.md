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
| `/clear` | `/reset` | 清空会话上下文和屏幕上的 transcript |
| `/quit` | `/exit` | 退出 codest |
| `/status` | — | 显示 backend、模式、transcript 大小和已加载插件 |

输入 `/` 就会在输入框上方弹出候选列表，继续输入会按指令名或别名前缀实时过滤（不区分大小写）：

- `↑` / `↓` 在候选之间循环选择，`Tab` 补全选中的指令；
- `Enter` 先补全再直接执行选中的指令，`Esc` 关闭候选但不影响已输入的内容；
- 候选只在输入还是“指令名”阶段出现；一旦输入空格或换行，就按普通文本处理。

只有整条输入的第一个非空白字符是 `/` 时才会被当作指令，其他文本照常发给模型。未知指令、多余参数或缺少参数会显示错误提示，不会发给模型。

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
| `codex-rs/core` 的会话循环 | `src/core/session.rs` | worker 持有会话历史与取消 token，产出事件 |
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
    │   ├── conversation.rs  # 线性会话历史
    │   ├── model.rs         # ModelClient trait + 错误类型
    │   ├── openai_compat.rs # HTTP + SSE 流式客户端
    │   ├── test_model.rs    # 仅 cargo test 编译的进程内假模型
    │   └── session.rs       # agent worker + AgentSession
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
        ├── chat.rs          # 历史单元格
        ├── composer.rs      # 输入编辑器
        ├── theme.rs         # 颜色
        └── wrap.rs          # 换行与光标映射
```

## 已知边界

- 只支持纯文本对话，没有工具调用、文件读写、审批、MCP、图片。
- 插件是编译期静态链接的：新增/修改插件需要改代码并重新编译，不支持运行时加载 `.so` 或脚本插件；插件与主程序同权限，没有沙箱隔离。
- 插件钩子在 TUI 线程同步执行，不能做阻塞 I/O。
- 没有会话持久化；退出即丢失。
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
