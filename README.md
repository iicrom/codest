# cst

`cst` 是一个从 [codex](https://github.com/openai/codex) 的 `tui` + `core` 架构中提取出来的**最小 TUI chat agent**。它只保留最基本的对话能力：输入消息、流式输出、取消当前回合、滚动历史，不包含工具调用、沙箱、审批、MCP、会话持久化等 codex 的完整能力。

项目位于 `../cst`（即 `codex` 的兄弟目录），与 codex 仓库完全独立。

## 快速开始

```bash
cd ../cst
cargo run                 # 没有 API key 时自动进入离线 mock 模型
cargo run -- --mock       # 强制使用离线 mock 模型（不联网）
cargo run -- --mock --tui-mode inline   # 可选：贴终端 scrollback 的 inline 模式
cargo test                # 全部是离线测试，不访问网络
python3 scripts/pty_smoke.py  # 伪终端冒烟测试：fullscreen / inline 两种模式都会跑
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
cargo run -- --provider openai --api-key sk-... --base-url https://api.openai.com/v1 --model gpt-4o-mini
```

> 按需求，本项目**没有做联网测试**。HTTP/SSE 客户端只做了编译与纯函数单元测试；真实网络行为需要在配置好 key 后自行验证。

## TUI 模式（fullscreen / inline）

和 pi 的 `tuiMode`（`fullscreen` / `regular`）以及 cht 的 `fullscreen` 设置一样，cst 支持两种显示模式，默认是 **fullscreen**：

| 模式 | 行为 |
| --- | --- |
| `fullscreen`（默认） | 进入 alternate screen，由应用持有整屏；transcript 用 `PgUp`/`PgDn` 或鼠标滚轮滚动 |
| `inline` | 不进入 alternate screen；完成的 transcript 直接写进终端自己的 scrollback，底部只保留输入框和状态行；`PgUp`/`PgDn` 交还终端 |

切换方式：

```bash
cst --tui-mode inline        # regular 是 inline 的别名
CST_TUI_MODE=inline cst      # 也可以用环境变量
```

`fullscreen` 会开启鼠标上报（滚轮滚动 transcript；`Alt`+滚轮一次 15 行）。需要终端原生选区时按住 `Shift` 拖拽。`inline` 不开启鼠标上报，保留终端原生 scrollback 和选区。

## 操作

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送消息 |
| `Ctrl+J` / `Alt+Enter` / `Shift+Enter` | 插入换行 |
| `↑` / `↓` | 多行时移动光标；单行时浏览历史输入 |
| `Ctrl+P` / `Ctrl+N` | 浏览历史输入 |
| `←` / `→` / `Home` / `End` | 光标移动 |
| `Backspace` / `Delete` | 删除字符 |
| `Ctrl+W` / `Alt+Backspace` | 删除前一个词 |
| `Ctrl+U` / `Ctrl+K` | 删除到行首 / 行尾 |
| 滚轮（fullscreen） | 滚动聊天记录（每格 3 行，`Alt`+滚轮 15 行） |
| `PageUp` / `PageDown`（fullscreen） | 滚动聊天记录；inline 下交给终端 scrollback |
| `Esc` | 中断正在生成的回复 |
| `Ctrl+C` | 生成中：中断；有空输入时第一下清空；输入为空时连按两次退出 |
| `Ctrl+D` | 输入为空时退出 |

## 从 codex 提取了什么

| codex 模块 | cst 对应模块 | 说明 |
| --- | --- | --- |
| `codex-rs/protocol`（SQ/EQ） | `src/core/protocol.rs` | `Op` / `Event` 消息总线，UI 不直接调用模型 |
| `codex-rs/core` 的会话循环 | `src/core/session.rs` | worker 持有会话历史与取消 token，产出事件 |
| `codex-rs/core/src/client.rs` | `src/core/model.rs` | 流式模型客户端抽象 |
| codex 的 Responses client | `src/core/openai_compat.rs` | 最小 OpenAI 兼容 Chat Completions + SSE 解析 |
| codex 的 fake/OSS provider | `src/core/mock.rs` | 离线确定性流式模型 |
| `codex-rs/tui/src/app.rs` | `src/tui/app.rs` | 终端事件循环、渲染、输入处理 |
| `codex-rs/tui/src/bottom_pane/textarea.rs` | `src/tui/composer.rs` | 多行输入、光标、换行映射 |
| `codex-rs/tui/src/history_cell*` | `src/tui/chat.rs` | 用户 / 助手 / 错误 / 提示 / 中断单元格 |
| codex 的 wrapping | `src/tui/wrap.rs` | Unicode 宽度感知换行与光标定位 |

codex 的 TUI 超过 10 万行、`core` 也有大量工具/沙箱/会话基础设施，且与 app-server 深度耦合，无法原样裁剪。这里提取的是**可独立编译的架构骨架**，而不是逐文件复制。

## 目录

```
cst/
├── Cargo.toml
├── README.md
├── scripts/
│   └── pty_smoke.py         # 离线端到端冒烟测试（pty）
└── src/
    ├── main.rs              # CLI + backend 选择
    ├── lib.rs
    ├── core/
    │   ├── protocol.rs      # Message / Op / Event
    │   ├── conversation.rs  # 线性会话历史
    │   ├── model.rs         # ModelClient trait + 错误类型
    │   ├── mock.rs          # 离线 mock 模型
    │   ├── openai_compat.rs # HTTP + SSE 流式客户端
    │   └── session.rs       # agent worker + AgentSession
    └── tui/
        ├── app.rs           # 事件循环 / fullscreen & inline 渲染
        ├── chat.rs          # 历史单元格
        ├── composer.rs      # 输入编辑器
        ├── theme.rs         # 颜色
        └── wrap.rs          # 换行与光标映射
```

## 已知边界

- 只支持纯文本对话，没有工具调用、文件读写、审批、MCP、图片。
- 没有会话持久化；退出即丢失。
- 没有网络重试、代理、认证刷新；HTTP 客户端只实现一次请求 + SSE 增量解析。
- `fullscreen` 只有滚轮滚动，应用内鼠标选区/复制尚未实现；选区用终端原生能力（通常按住 `Shift` 拖拽）。
- `inline` 下已经写进 scrollback 的行不会随窗口宽度重新换行。
- 测试全部离线：`cargo test` 不会发起任何网络请求。

## 后续可扩展点

1. 在 `Op` 中加入工具调用与审批事件，向 codex 的 SQ/EQ 靠拢。
2. 为 `Conversation` 增加 token 预算与压缩。
3. 会话持久化（rollout JSONL）与 resume。
4. Markdown / 代码高亮渲染（codex 使用 `pulldown-cmark` + `syntect`）。
5. `fullscreen` 下的应用内鼠标拖选与 OSC 52 复制（对齐 pi/cht）。
