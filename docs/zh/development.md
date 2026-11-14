# 开发

[English](../development.md) | 简体中文

这里的一切都离线运行。`cargo test` 使用进程内假模型、不访问网络；需要端点的冒烟脚本各自在本机回环端口起一个 OpenAI 兼容测试服务。构建用 `cargo build`（Rust 1.88+，edition 2024）；冒烟脚本只需要一个 debug 二进制。

## 构建与运行

```bash
cargo build                 # 生成 target/debug/codest 与 target/debug/cst（别名二进制）
cargo run -- --help         # default-run 是 `codest`
cargo install --path .      # 两个命令都装进 ~/.cargo/bin
```

没有 feature 矩阵可选，也没有 build script：crate 直接按 `Cargo.toml` 里声明的依赖编译。唯一的平台条件依赖是 Unix 上的 `libc`，供 shell 工具的进程组终止使用；Windows 路径不需要额外 crate（`taskkill` 是系统自带程序）。冒烟脚本需要的东西只有 debug 二进制和 `python3`（仅标准库）。

三个 `scripts/*_smoke.py` 都需要 unix 伪终端（`pty` / `fcntl`），所以只在 unix 宿主上可跑。Windows 上的检查手段是 `cargo test`：工具单元测试会真实拉起 `pwsh`，断言退出码、stderr 合并、超时杀进程树与 UTF-8 输出。

## 测试

```bash
cargo test                                      # 单元测试（离线，进程内假模型）

python3 scripts/openai_test_server.py --port 8080   # 本地 OpenAI 兼容服务
python3 scripts/pty_smoke.py                    # pty 内端到端冒烟（双 TUI 模式）
python3 scripts/tool_smoke.py                   # 真实 shell/read 执行 + /tools 开关
python3 scripts/markdown_smoke.py               # Markdown 渲染可视检查（双 TUI 模式）
python3 scripts/session_header_smoke.py         # 断言 session-id 头真的发到线上
python3 scripts/plugin_smoke.py                 # 外部插件安装/构建往返
```

| 命令 | 断言内容 |
| --- | --- |
| `cargo test` | core 与 TUI 各模块的单元测试。模型是 `EchoModelClient`，一个进程内的确定性 echo，按词大小的分片流式输出，什么都不出进程。会话测试在总线上驱动真实的 worker，断言事件序列、取消、工具轮与 rollout 内容。 |
| `pty_smoke.py` | 在伪终端里启动二进制、输入消息，把捕获的 ANSI 输出渲染成纯文本网格。默认跑两种 TUI 模式：`fullscreen` 必须发出 `\x1b[?1049h`（alternate screen）与 `\x1b[?1000h`（鼠标捕获），`inline` 两者都不能发；滚轮必须能滚、消息必须出现在 transcript、模式提示与空闲 footer 必须出现、进程必须干净退出。config 变体额外检查模型发现、菜单切换与 footer 的上下文窗口（声明窗口与发现窗口）；两个变体都会用 `--continue` 再跑一遍，检查恢复的会话与记录的用量。 |
| `tool_smoke.py` | 对本地服务跑 shell 工具与 `read` 两个用例：真实工具轮必须显示出命令输出（`hello-from-tool`、`exit code: 0`）或文件内容（`alpha`、`gamma`、`3 lines total`）；随后 `/tools <name> off` 加 `/context` 必须按收窄后的集合报出 `Tools (3/4 enabled)`，并仍把该工具列为 `disabled`。 |
| `markdown_smoke.py` | 两种模式下的可视检查，素材来自脚本化的服务回复：标题文本、粗体/斜体/行内代码、圆点与嵌套圆点标记、有序标记、`▎` 引用竖条、分隔线、带 `│ rust` / `│ python` 标签与正文的两个代码围栏、表头与表线、任务复选框——以及屏幕上不出现原始的 `**bold**` 标记。inline 模式检查标题、代码标签与无原始标记三项。 |
| `session_header_smoke.py` | 对会 dump 请求头的服务跑三种情况：默认实例发出的 `session-id` 头取值匹配 UUIDv7；`wire.session_header = "none"` 一个都不发；model 级 opt-out 也不发。客户端完全没发请求也算失败。 |
| `plugin_smoke.py` | 外部插件往返，默认指向同级 checkout（`--plugin ../cst-provider-qoder`）：`codest plugin install <路径>` 干净退出、写入 `plugins.toml`、生成 wrapper crate 并构建出 wrapper 二进制；manifest 以 path 依赖该插件，生成的代码链接它的 `PLUGIN` 静态量；构建出的二进制在 `/status` 里报出插件 id、在 `/help` 里列出其指令；`codest plugin uninstall` 重新生成不含插件的 wrapper 并重新构建。最后对 codest checkout 做一次 `git status --porcelain` 比对，证明安装没有动过仓库（`--skip-git-check` 可关闭）。 |

TUI、工具、markdown 与会话头这几个脚本各自起停自己的测试服务、用临时回环端口（`plugin_smoke.py` 例外，它构建的是 wrapper crate），所以上面单独一行启动 `openai_test_server.py` 只是手动调试时用。所有脚本都接受二进制路径参数，默认 `target/debug/codest`（`session_header_smoke.py` 用 `--binary`，默认 `target/debug/cst`）。

### 单元测试在哪

测试与被测代码放在一起，位于各模块底部的 `#[cfg(test)] mod tests`——目前有 29 个模块带测试模块。同步测试是普通 `#[test]`；worker 与 TUI 循环的测试是 `#[tokio::test]`，用 `EchoModelClient` 在 `Op`/`Event` 总线上驱动真实的 `AgentSession`，走的就是真实前端的路径。模型客户端以下几乎没有 mock，会话测试断言的是真实的事件序列（`TurnStarted` … `TurnUsage` … `TurnComplete`），而不是内部状态。

覆盖最重的模块正是出错时最安静的那些：`core/session.rs`（回合、取消、工具轮、会话切换）、`core/rollout.rs`（回放、清空、重命名、上下文检查点）、`core/config.rs`（严格解析、`$VAR` 模板、wire quirks）、`tui/markdown.rs`（每种语法结构、换行、表格压缩）、`tui/chat.rs`（卡片渲染与折叠）、`tui/app.rs`（模式装配、footer、菜单、`/provider` 接线）。

依赖拉取完成之后 `cargo test` 不再需要网络；运行本身完全在进程内。

改 worker 之前值得先扫一眼会话测试：它们的名字就是规格。代表性的一批：`streams_a_turn_end_to_end`、`tool_calls_execute_and_continue_the_turn`、`set_tools_enabled_withholds_declarations_and_rejects_calls`、`interrupt_lands_during_a_slow_tool_call`、`interrupt_keeps_partial_output`、`an_abort_mid_tool_call_closes_the_round_with_an_interrupted_result`、`a_runaway_tool_loop_hits_the_configured_round_limit`、`thinking_is_recorded_for_resume_but_stays_out_of_the_conversation`、`new_session_starts_fresh_context_and_switch_restores_it`、`a_fresh_session_has_an_id_before_it_has_a_file`、`switching_sessions_switches_the_id_sent_upstream`、`switch_model_swaps_the_backend_between_turns`、`an_empty_route_answers_turns_with_guidance`、`switching_from_an_empty_route_arms_the_backend`。它们共同钉住了架构赖以成立的两条不变量：回合的事件序列，以及只有 `Op` 能改变 worker 状态。

### 单独跑一个脚本

```bash
cargo build                                     # 生成 target/debug/codest 与 target/debug/cst
python3 scripts/pty_smoke.py --mode inline      # 只跑一种模式
python3 scripts/tool_smoke.py target/release/codest
python3 scripts/session_header_smoke.py --binary target/debug/cst
python3 scripts/plugin_smoke.py --skip-git-check
```

每个脚本对每条断言打印一行 `PASS`/`FAIL`，有任何失败就以非零退出码返回，CI 一步只需 `cargo test && python3 scripts/...`。

### 测试服务

`scripts/openai_test_server.py` 只依赖标准库（Python 3，无第三方包），参数有 `--host`、`--port`（0 表示临时端口）、`--delay`（流式 token 之间的秒数，默认 0.02）、`--model`、`--tool`、`--tool-arguments`、`--dump-headers`（每个请求追加一行 JSON）、`--quiet`。它提供：

- `POST /chat/completions` —— 收 OpenAI 形状的请求，回 SSE 流（`data:` 帧，最后是 `data: [DONE]`），末尾带一个 usage 分片。推理增量先于可见回复发出，模拟 `reasoning_content` 先到的推理型后端；
- `GET /models` —— OpenAI 形状的模型目录，让 `fetch_models` 发现有东西可发现；
- 关键词触发 —— `@bash`（Windows 上为 `@pwsh`，跟随宿主的 shell 工具）、`@read`、`@write`、`@edit`、`@ask-user` 映射到对应内置工具、参数取关键词之后的文本（`@bash ls -la` → `bash` 收 `{"command": "ls -la"}`）；`@write <路径> <内容>`、`@edit <路径> | <旧> | <新>` 与 `@ask-user <问题> | <选项> | <选项>`（2-4 个选项，经问题对话框作答）遵循固定形状；`@tool` 调用 `--tool` / `--tool-arguments` 指定的那个；
- 普通消息 —— 不含关键词的消息得到一条 help 回复，列出这些关键词与端点。

服务在请求之间不保存状态：它靠检查收到的消息来决定怎么回答。已经带了一轮工具调用的回合会得到轮次总结回复而不是再次调用——没有这道闸，关键词会一直留在用户消息里，循环永远不会结束。正是这一条规则让冒烟测试能在无外网、无真实模型的情况下跑完整个 agent 链路：请求、工具调用、工具结果、后续回合。

## 加一个测试

- **Core 行为** —— 在 `src/core/session.rs` 的测试模块里加 `#[tokio::test]`：用 `EchoModelClient` 构建 `AgentSession`（断言请求形状时用一个小 recording client），提交 op，收集事件直到 `TurnComplete`。该模块里已有 `collect_turn` / `collect_until` 辅助函数。
- **渲染** —— 在渲染该内容的模块里加普通 `#[test]`：Markdown 在 `tui/markdown.rs`，单元格在 `tui/chat.rs`，换行在 `tui/wrap.rs`。断言 `plain(&lines)`（拉平的文本），让测试描述用户看到的东西，而不是 span 结构。
- **端到端行为** —— 在对应的冒烟脚本里加断言。它跑真实二进制与真实事件循环，能抓到单元测试抓不到的接线错误；TUI bug 的回归测试因此也应该落在 `pty_smoke.py`。

## 仓库约定

- **布局。** 二进制是 `codest`（`src/main.rs`）加 `cst` 别名（`src/bin/cst.rs`），外加 `crates/cst-plugin-api` 这个 workspace 成员。Edition 2024，`rust-version = "1.88"`，resolver 3。
- **Dev profile。** `[profile.dev]` 设 `opt-level = 0` 与 `debug = "line-tables-only"`；较短的调试信息让增量构建保持轻快，冒烟脚本驱动的正是 debug 二进制。
- **没有单独 lint 配置。** 仓库里没有 `rustfmt.toml`、`clippy.toml` 或 CI workflow；`cargo fmt` 与 `cargo clippy` 走默认配置，少数刻意的例外（`large_enum_variant`、`too_many_arguments`）是带理由的局部 `#[allow]`。
- **构造上离线。** 任何测试——单元的或冒烟的——都不访问公网。需要服务的都起在 `127.0.0.1` 的 0 号端口并读回实际端口。
- **状态隔离。** 每个脚本把 `CODEST_HOME` 指向新的临时目录（`$CODEST_HOME`，否则 `$HOME/.codest` 才是真实根目录），既避免碰开发者的状态，也让 `--continue` 用例可复现。

## 新增插件或改文档

插件是链接进二进制的 Rust 代码。要新增插件，从 [plugins.md](../plugins.md) 开始：`Plugin` trait、effect 词表、作为示例的内置插件、装配期校验规则，以及为什么注册表是一个静态数组。如果只是安装来自其他仓库的插件而不在这里编写，[local-plugins.md](../local-plugins.md) 讲仓库外的 wrapper crate、生成的 crate 布局与契约版本规则。

文档双语维护：`docs/*.md` 为主版本，`docs/zh/*.md` 逐节镜像，表格与代码块完全一致。写在文档里的事实，落笔前应先对照源码核实；模块级文档注释通常是对意图最准确的表述。

## 调试失败的冒烟测试

- 先手动跑一遍：起 `openai_test_server.py --port 8080`，再用 `--api-key test --base-url http://127.0.0.1:8080/v1 --model test-model` 启动 codest。交互下链路正常，说明问题在测试的期望而不是代码。
- `pty_smoke.py` 与 `markdown_smoke.py` 会在失败断言旁打印渲染出的屏幕；`tool_smoke.py` 在缺 needle 时把屏幕打到 stderr。先看这里——捕获的网格通常直接指出缺了哪一行。
- 查请求头时，用 `--dump-headers` 起服务再读 JSON 行：头的存在与拼写可以直接核对。
- 卡住的 TUI 测试通常是在等一个永远不会出现的空闲 footer；先确认回合真的到了 `TurnComplete`，再怀疑渲染。

## 已知边界

- 只支持纯文本对话：没有图片输入、没有审批、没有 MCP。
- Markdown 渲染是精选子集：没有 OSC 8 超链接、没有表格 key/value 降级、没有 mermaid 和数学公式；图片只显示 alt 文本与地址。
- 插件是编译期静态链接：新增/修改需要重新编译；不支持运行时加载 `.so` 或脚本插件。插件与宿主同权限——没有沙箱。
- 插件钩子在 TUI 线程同步执行；钩子里不能做阻塞 I/O。
- 会话没有并发写保护（两个 codest 实例写同一工作区可能交错）、没有归档/删除/搜索、没有跨工作区迁移。
- HTTP 客户端只做一次请求 + SSE 解析：没有重试、代理、认证刷新。API key 支持 `$VAR` 模板，但没有 OAuth/凭据存储；token 轮换需在协议工厂内自行解决。
- 未配置 provider 时空路由启动：发消息得到添加指引而不是发出请求。
- `fullscreen` 下没有应用内鼠标选区/复制；使用终端原生选区（通常按住 `Shift` 拖拽）。
- `inline` 下已写入 scrollback 的行不随窗口宽度重新换行。

## 后续计划

1. `Conversation` 的 token 预算与压缩（由 `context_window` 与每轮用量驱动）。
2. `/effort` 写回 `config.toml` 作为全局默认。
3. `/provider` 中删除 provider（`ModelPool::remove_instance()` 已就绪，尚未接菜单）；用 `toml_edit` 保留注释地写回。
4. Markdown：OSC 8 超链接与表格 key/value 降级。
5. `fullscreen` 下的应用内鼠标拖选与 OSC 52 复制。
6. 用 `inventory`/`linkme` 做分布式静态注册（`Plugin` 契约不变）。
7. 等 `cst-plugin-api` 发布后：可选的插件签名与版本约束求解。

## 参与贡献

欢迎 issue 与 PR。提交 PR 请：

1. 保证 `cargo test` 通过（完全离线）；
2. 涉及用户可见流程时，补充或更新对应的冒烟脚本；
3. 遵循 [Conventional Commits](https://www.conventionalcommits.org)——本仓库历史使用 `feat:` / `fix:` / `chore:` / `test:` 前缀。

反馈渲染或 TUI 问题时，附上 `scripts/markdown_smoke.py` 或 `scripts/pty_smoke.py` 的复现会很有帮助。两个脚本都会把捕获的屏幕渲染成纯文本、把每条断言按名字打印为 `PASS` 或 `FAIL`，失败的检查通常能直接从输出里读出来；在脚本里加一条断言也是修复的天然回归测试。

## 相关文档

- [architecture.md](../architecture.md) —— 模块地图、`Op`/`Event` 总线，以及测试所覆盖的回合流程。
- [plugins.md](../plugins.md) —— 编写与注册插件。
- [local-plugins.md](../local-plugins.md) —— 安装来自其他仓库的插件。
- [tui.md](../tui.md) —— pty 与 markdown 冒烟脚本在检查什么。
- [../README.md](../../README.md) —— 安装与快速开始。
- [../scripts/openai_test_server.py](../../scripts/openai_test_server.py) —— 离线测试服务。

[← 返回 README](../../README.md)
