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
python3 scripts/tool_smoke.py # 工具冒烟测试：真实执行 bash / read 并验证 /tools 开关
python3 scripts/markdown_smoke.py # Markdown 渲染可视检查：标题/列表/表格/代码块/引用
```

连接一个 OpenAI 兼容的流式接口（`CST_BASE_URL`/`CST_API_KEY` 需成对提供，只给其一时 codest 会在发送时提示补全）。无论是否指定 `--model`，codest 都会对该端点做模型发现（尽力而为）：`/model` 菜单补全端点的模型列表，窗口长度等元数据（footer 的上下文显示）也从发现结果带入：

```bash
export CST_API_KEY=sk-...
export CST_BASE_URL=https://api.openai.com/v1
export CST_MODEL=gpt-4o-mini                   # 可选；不给则由 /model 从该端点发现模型列表
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
>
> 上面的 flag/env 方式是**单端点兼容路径**；配置了 `~/.codest/providers.toml` 之后，它们退化为"覆盖所选实例字段"。多 Provider 的完整用法见[下一节](#多-provider)。

### 本地测试服务

`scripts/openai_test_server.py` 是只依赖 Python 标准库的简易 OpenAI Chat Completions 服务端，用于在没有 key、没有外网的情况下端到端验证 TUI 和 SSE 客户端：

- `POST /chat/completions` 支持 `"stream": true` 的 SSE 增量输出，也支持普通 JSON 响应；流式响应末尾附带 usage（`prompt_tokens: 42 / completion_tokens: 7`），供 footer 与 rollout 的用量链路验证；
- `GET /models` 返回 OpenAI 形状的模型目录（供 `fetch_models` 发现），`test-model` 带 `context_length`（OpenRouter 拼写）、`second-model` 带 `context_window`，两种字段拼写都覆盖；
- `GET /health` 返回 `{"status":"ok"}`；
- 不校验 API key；默认回复是 help（列出可触发工具的关键词），消息里带关键词时改为对应工具调用；
- `--delay` 控制 token 间隔，`--port 0` 使用随机端口并打印实际地址。

```bash
python3 scripts/openai_test_server.py --port 0
# codest test server listening on http://127.0.0.1:xxxxx/v1
```

> 按需求，本项目**没有做真实外网测试**。`cargo test` 是纯离线单元测试；`scripts/pty_smoke.py` 只访问本机启动的测试服务。

## 多 Provider

codest 支持多个 provider 实例，每个实例挂多个模型；协议与实例分离——"自定义端点"只是一个 `base_url` 不同的 `openai-compat` 实例，真正的自定义线协议由插件贡献。完整设计见 [`docs/multi-provider.md`](docs/multi-provider.md)。

### 配置

`~/.codest/config.toml` 存全局设置（本轮仅 `default` 一项），provider 实例放 `~/.codest/providers.toml`：

```toml
# ~/.codest/providers.toml
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"        # $VAR 模板，密钥不必落盘
fetch_models = true                # 从 GET /models 发现模型列表

  [[providers.models]]             # 也可以显式声明（优先于发现结果）
  id = "gpt-4o-mini"

[[providers]]
name = "local"
base_url = "http://127.0.0.1:8080/v1"
api_key = "test"
headers = { "X-Custom" = "v" }     # 静态请求头/请求体覆盖：多数"特殊端点"免插件
body_overrides = { temperature = 0.5 }

  [providers.wire]                 # 字段拼写级 quirk（受控词汇，自由字段名）
  thinking_fields = ["reasoning"]  # 该端点的思考文本字段（默认 reasoning_content）
  effort_field = "none"            # /effort 的请求字段名；"none" = 从不下发
```

```toml
# ~/.codest/profiles.toml —— 命名的会话预设（一组"模型+effort+工具+提示词"）
[profiles.code]
default = "openai/gpt-4o-mini"     # 初始选择
effort = "high"                    # 初始 /effort（"off" = 不下发 effort 字段）
tools = ["prompt_debug"]           # 只声明这些插件的工具；缺省 = 全部；空表 = 无工具

  [profiles.code.prompt]
  append = "你是资深 Rust 工程师…"  # 追加到 system prompt 末尾（/context 归属 profile:<名>）

[profiles.fast]
default = "local/qwen2.5"
effort = "low"
```

- **没有内置默认 provider,也没有默认 base_url/model**:flag/env 提供的 `base_url` 与 `api_key` 按**成对**要求合成单端点实例——只给了其中一个时,codest 照常启动并在发送消息或 `/model` 获取列表时提示补全另一个;两者都未提供则为空路由(见下)。显式给了 model 就直接可用;没给 model 则实例不带模型列表,由 `/model` 从该 `base_url` 的 models API 发现(启动时即后台拉取);
- 文件存在但解析失败会启动报错（带路径与细节），未知字段同样报错；
- 起始选择优先级：`--provider`/`--model` > `--profile` 的 `default` > config.toml `default` > 第一个实例的第一个模型；profile 是**预设**而非第二配置层——flag 仍可覆盖其任意字段，恢复的会话恢复自己的状态；
- `--profile <名>` 启动；`/profile` 在会话内列出/切换（在回合边界生效，rollout 记 `profile_switched`）；profile 的工具集与提示词属装配期，切换只应用其 `default`/`effort`；
- 也可以用 `protocol = "test"` 选内置的进程内 echo 后端，完全离线试玩。设计见 [`docs/profiles.md`](docs/profiles.md)。

### `/model` 切换

| 指令 | 行为 |
| --- | --- |
| `/model` | 打开模型菜单：`↑`/`↓` 选择、`Enter` 切换、`r` 重新发现模型、`Esc` 关闭 |

- 菜单按 provider 分组列出全部可选的 `provider/model`，当前项标 `current`；
- 切换只在回合边界生效（不打断正在生成的回复），footer 的 backend 标签即时更新；
- 开启了 `fetch_models` 的实例在启动时后台发现模型，结果合并进菜单并缓存到 `~/.codest/model-cache.json`（失败只产生一条通知，绝不阻塞会话）；
- 会话 rollout 记录初始与切换后的模型（`SessionMeta` 与 `model_switched` 行），旧 rollout 文件照常加载。

### `/provider` 添加 provider 与登录

| 指令 | 行为 |
| --- | --- |
| `/provider` | 打开 provider 向导：新增 OpenAI 兼容端点、登录插件贡献的 provider、查看已配置列表 |

- **Add an OpenAI-compatible provider**：依次填 route name、base URL、API key（可空，支持 `$VAR`）、model id（可空 = 启动时发现）；进入确认屏前探测一次 `GET /models`，探测失败不阻塞写入；
- **Sign in to a plugin provider**：只有插件贡献了登录入口时才出现。宿主按协议把已配置实例与插件配对，登录状态读自插件的本地凭据；浏览器/设备流授权后自动完成，`Esc` 取消；
- 确认后先原子写回 `~/.codest/providers.toml`，再让新 provider 立即出现在 `/model`，随后后台发现模型；
- 写回会丢失文件里的注释与排版（不引入 `toml_edit`），所以向导只在明确添加时才写。

细节见 `docs/provider.md`。

### 插件贡献自定义协议

端点说 OpenAI 方言时**不需要写代码**（配置 `headers` / `body_overrides` 即可）；请求体结构、流格式、签名机制不同时，插件实现 `ProtocolFactory`（编译期）并在 `providers.toml` 里以 `protocol = "<id>"` 引用：

```rust
impl Plugin for Vendor {
    fn id(&self) -> &'static str { "vendor" }
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        std::slice::from_ref(&VENDOR_FACTORY)
    }
}
```

```toml
[[providers]]
name = "vendored"
protocol = "vendor-protocol"
api_key = "$VENDOR_TOKEN"
models = [{ id = "vendor-large" }]
```

协议插件与命令插件一样是编译期静态链接的：它给的是维护者的模块化，不是运行时可扩展性。工厂示例与扩展契约见 [`docs/multi-provider.md`](docs/multi-provider.md) 第 8 节。

第三方插件仓库可以独立维护：契约类型在 `crates/cst-plugin-api`，插件依赖它，宿主依赖插件（所以没有 Cargo 循环依赖）。接入是一条命令，**不改本仓库源码**：

```bash
./target/debug/codest plugin install ../my-plugin   # 本地 checkout；也支持 --git <url> 或 crates.io 包名
```

它把插件记进 `~/.codest/plugins.toml`，在 `~/.codest/plugins/crate/` 生成一个依赖宿主与插件的独立 crate 并在那里 `cargo build`；`codest plugin list` 查看已装项，`codest plugin uninstall <name>` 移除，`codest plugin build [--release|--install]` 重建。因为生成物住在仓库外，`git status` 始终干净（不再需要 `git update-index --skip-worktree`）。

完整步骤见 [`docs/local-plugins.md`](docs/local-plugins.md)；参考实现是 [`../cst-provider-qoder`](../cst-provider-qoder)（Qoder 的 COSY 签名协议 + 浏览器/PAT 登录）。

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

## 回答的 Markdown 渲染

助手回答按 Markdown 渲染（用户输入、通知、错误和思考内容仍是纯文本）。解析用 `pulldown-cmark`，与 codex 的 `tui/src/markdown_render.rs` 同一套事件流，但只保留聊天里真正用得到的那一层：

| 语法 | 渲染 |
| --- | --- |
| `#`–`######` 标题 | 逐级变静：H1 强调色加粗下划线 → H2 强调色加粗 → H3 加粗 → H4 加粗斜体 → H5/H6 灰色 |
| 段落 / 软换行 | 按终端宽度换行；空行分段，软换行不断段 |
| `-` / `1.` 列表、嵌套 | 标记用强调色，续行悬挂在标记右侧；每层缩进一个标记宽 |
| `> 引用` | `▎` 竖条，可嵌套 |
| `---` | `───` 分隔线 |
| 围栏 / 缩进代码块 | `│` 竖条；围栏带语言名时先出一行暗色标签，并按该语言做语法高亮；代码硬换行（不按空格折行、不丢字符），续行保持缩进；空行原样保留 |
| 表格 | 按内容对齐成列，表头加粗，下面一条 `─` 分隔；超宽时优先压缩最宽的列，每列最少 3 列宽 |
| `**粗**`、`*斜*`、`~~删除~~`、`` `代码` `` | 对应样式；行内代码用强调色 |
| `[文字](url)` | 文字带下划线强调色，地址以暗色跟在括号里（autolink 不重复） |
| `- [x]` / `- [ ]` | 渲染为 `[x]` / `[ ]` 复选框 |
| 图片 | `🖼` 加 alt 文本和地址 |

流式输出的关键点是"哪些行已经不会再变"：终端 scrollback 写进去就无法改写，因此只有已定型的行会落到 scrollback，正在生长的最后一个顶层块留在底部 pane 里反复重绘（未闭合的 `**`、半张表格、还没收尾的代码围栏都会改变已输出的行）。`fullscreen` 因为每帧整屏重绘，始终按最新内容渲染。

代码高亮的引擎是 `syntect`（`regex-fancy` 纯 Rust 正则，不需要 oniguruma C 工具链），语法集取自 `two-face`（bat 的语法包，补上了 syntect 自带集合里缺的 TypeScript、TOML、Dockerfile 等）。渲染层只认 `src/tui/highlight.rs` 里的 `TokenKind`（keyword / type / function / string / number / comment / constant / attribute），不认引擎类型：换引擎只要另写一个 `Highlighter`，`markdown.rs` 不动。颜色由 `theme.rs` 的 `CODE_*` 逐项映射。未知语言或超过 128 KiB 的块退化为纯文本，但仍会显示标签。高亮按整块 tokenize（多行字符串、块注释要跨行才有正确状态），结果按内容哈希缓存——transcript 每帧都会重渲染历史单元格，没有缓存的话一个已定型的代码块会一直重复 tokenize。

跑一遍可视检查（起一个只回答 Markdown 的本机服务，两种 TUI 模式各跑一轮）：

```bash
cargo build
python3 scripts/markdown_smoke.py
```

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
| `/context` | — | 显示提供给模型的内容：**工具及启用状态**（`Tools (n/m enabled)`，逐个标注 `enabled`/`disabled`/`read-only`）、组装后的系统提示词（逐段标注出处：base / 各插件）、请求配置（endpoint、effort、`body_overrides`）和 token 用量。输出较长，打开时会停在报告开头，`PgDn`/滚轮继续 |
| `/new` | — | 归档当前对话，开启一个新的空会话 |
| `/rename <name>` | — | 重命名当前会话；默认名取该会话第一句用户输入 |
| `/sessions` | `/resume` | 打开会话列表菜单，切换并继续以前的会话 |
| `/model` | — | 打开模型菜单，在 `provider/model` 之间切换（见"多 Provider"） |
| `/provider` | — | 打开 provider 向导：新增 OpenAI 兼容端点、登录插件贡献的 provider、查看已配置列表（见"`/provider`"） |
| `/effort [level]` | — | 查看或设置模型推理等级：无参数打开 `↑`/`↓` 选择菜单，`/effort <level>` 直接设置，`off` 清除；随请求以 `reasoning_effort` 字段发送。等级表来自模型的 `reasoning` 声明或 models API 的 `default_level`（选中模型时自动采纳为初始值） |
| `/thinking` | — | 切换模型思考过程（thinking 输出）的折叠/展开 |
| `/tools [name] [on\|off]` | — | 查看或设置本会话启用的工具：无参数打开勾选菜单（`空格` 切换、`Enter` 应用、`Esc` 关闭），`/tools <name>` 直接翻转单个工具，`on`/`off` 固定方向。禁用的工具不再声明给模型，模型误调用也会被拒绝。开关按会话记录，恢复会话时一并还原（见"工具调用"） |

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
- 插件可以贡献 slash 命令、系统提示词片段、`on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` 钩子、工具（`tools()` 声明 + `run_tool()` 执行），以及多 Provider 的协议工厂（`protocols()`，见"多 Provider"）。
- 插件不直接操作终端或会话，只返回 `PluginEffect`（通知、错误、报告块、清屏、退出、设置输入框、提交新回合），由 TUI 统一应用；因此插件代码不依赖 ratatui，也可以被其他前端复用。
- 重复插件 id、重复命令名/别名、重复工具名、非法命令名会在启动时被拒绝。
- 外部插件（第三方仓库）通过 `codest plugin install` 装进一个仓库外生成的 wrapper crate，不需要改动本仓库；`PluginHost::shipped()` 始终只含内置插件，是测试用的稳定表。

内置的 `/help`、`/clear`、`/quit`、`/status`、`/context` 本身就是插件，可以作为示例。完整契约、执行顺序、错误处理和“新增插件”的完整代码见 [`docs/plugins.md`](docs/plugins.md)；外部插件的安装与契约版本规则见 [`docs/local-plugins.md`](docs/local-plugins.md)。

新增插件的最小步骤：

1. 在 `src/plugins/builtin/` 下新建模块，声明 `static` 插件值和 `CommandSpec`；
2. 实现 `Plugin`（至少 `id`，按需实现 `commands` / `run_command` / 钩子 / 工具）；
3. 在 `src/plugins/builtin/mod.rs` 的 `BUILTIN_PLUGINS` 里加一项；
4. 运行 `cargo test`。

### 工具调用（tool calling）

本体实现了 openai-completions 的完整工具链路，具体工具由插件添加：

1. 插件用 `Plugin::tools()` 声明 `ToolSpec`（名称、描述、JSON Schema 参数，
   以及 `read_only` 标记——纯查看类工具的提示，供 `/tools` 展示与未来的
   只读 profile 使用）；
2. 请求自动带上 `tools` 字段；模型流式返回 `tool_calls` 后，worker 按 name
   派发到声明该工具的插件的 `run_tool()`；
3. 执行结果（或 `error: …` 失败文本）作为 `role: "tool"` 消息回传，回合继续
   （上限 8 轮，`Esc` 可中断）；每次调用在 transcript 上渲染为一个 `⚙` 单元，
   会话持久化与恢复都覆盖工具消息；
4. 用 `/context` 可以随时查看当前提供了哪些工具。

内置两个工具插件（`src/plugins/builtin/tools/`）：

- **bash**（id `bash`）：单次 shell 执行，`$SHELL -c`，默认 300s 超时（可传
  `timeout`，上限 600s），超时杀死整个进程组；输出合并 stderr、带退出码脚注，
  按尾截断（最后 2000 行 / 50 KB，参考 pi）。
- **fs**（id `fs`）：`read`（行号窗口，`offset`/`limit` 续读，二进制探测）、
  `write`（整文件写入，自动建父目录）、`edit`（精确字符串替换，要求唯一匹配或
  `replace_all`，保留 BOM 与 CRLF，语义与 dsh 一致）。三者共享工作目录解析与
  截断策略；`read` 带 `read_only` 标记。

所有工具都支持**会话级开关**（`/tools`）：禁用的工具从模型请求的 `tools`
字段里移除（省 token 也防幻觉调用），模型误调用会在执行期被拒绝；开关随
rollout 记录并在恢复会话时还原。这也是未来 readonly / normal profile 的
落点：profile 按插件 id 粗筛（装配期），`/tools` 按工具名细调（会话期）。

skills、workspace instructions 等上下文不需要专门的机制：作为普通插件的
`system_prompt()` 片段注入即可，`/context` 会逐段标注出处。

没有真实工具也能验证链路：用 `scripts/openai_test_server.py` 起测试服务后，消息里
的**关键词**会触发一次 tool_call（参数分片流式下发）：`@bash <命令>`、
`@read <路径>`、`@write <路径> <内容>`、`@edit <路径> | <旧> | <新>` 分别调用对应的
内置工具，`@tool` 则调用 `--tool` / `--tool-arguments` 指定的工具（默认 `ping`，
本体未注册该工具，会以 `unknown tool` 错误结果回传）。结果回传后下一轮回复会复述
工具结果，完成整条回环；**不带关键词的消息返回 help**，列出全部关键词与用法。

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
| `codex-rs/tui/src/markdown_render.rs` | `src/tui/markdown.rs` | Markdown → ratatui 行（代码高亮见 `highlight.rs`；无表格 key-value 降级 / mermaid） |
| codex 的 wrapping | `src/tui/wrap.rs` | Unicode 宽度感知换行与光标定位 |

codex 的 TUI 超过 10 万行、`core` 也有大量工具/沙箱/会话基础设施，且与 app-server 深度耦合，无法原样裁剪。这里提取的是**可独立编译的架构骨架**，而不是逐文件复制。

## 目录

```
codest/
├── Cargo.toml
├── README.md
├── docs/
│   ├── plugins.md           # 静态插件系统设计
│   └── multi-provider.md    # 多 Provider 框架设计
├── scripts/
│   ├── openai_test_server.py # 本地 OpenAI 兼容测试服务（关键词 @bash/@read/@write/@edit/@tool 触发工具调用，默认回复 help）
│   ├── pty_smoke.py          # 离线端到端冒烟测试（pty）
│   └── tool_smoke.py         # 内置工具与 /tools 的端到端冒烟测试
└── src/
    ├── main.rs              # codest 主入口
    ├── bin/cst.rs           # cst 别名入口
    ├── cli.rs               # CLI 参数 + 启动装配（配置文件、注册表、初始选择）
    ├── lib.rs
    ├── core/
    │   ├── protocol.rs      # Message / Op / Event
    │   ├── commands.rs      # 命令表 / 解析（指令由插件提供）
    │   ├── sessions.rs      # 内置 /new /rename /sessions 命令 spec（多会话）
    │   ├── reasoning.rs     # 内置 /effort /thinking 命令 spec + 推理等级校验
    │   ├── conversation.rs  # 线性会话历史
    │   ├── model.rs         # ModelClient trait + StreamItem + 错误类型
    │   ├── openai_compat.rs # HTTP + SSE 流式客户端（openai-compat 协议）
    │   ├── sse.rs           # 共享 SSE 帧解析（所有 SSE 协议复用）
    │   ├── config.rs        # config.toml / providers.toml / profiles.toml 解析 + $ENV 模板
    │   ├── providers.rs     # 协议工厂 / 注册表 / 模型池 / /model 命令 spec
    │   ├── rollout.rs       # 会话持久化（JSONL rollout 落盘 / 回放）
    │   ├── echo_model.rs    # 进程内 echo 模型（测试 + test 协议后端）
    │   └── session.rs       # agent worker（持有全部会话）+ AgentSession
    ├── plugins/
    │   ├── mod.rs           # Plugin trait / PluginHost / effects / 协议收集
    │   ├── install.rs       # codest plugin install|uninstall|list|build
    │   └── builtin/
    │       ├── mod.rs       # 静态注册表
    │       ├── help.rs      # /help
    │       ├── clear.rs     # /clear
    │       ├── quit.rs      # /quit
    │       └── status.rs    # /status
    └── tui/
        ├── app.rs           # 事件循环 / fullscreen & inline 渲染 / /model 菜单
        ├── session.rs       # 每会话视图状态（SessionView）
        ├── chat.rs          # 历史单元格
        ├── composer.rs      # 输入编辑器
        ├── markdown.rs      # 助手回答的 Markdown 渲染（pulldown-cmark）
        ├── highlight.rs     # 代码块语法高亮（syntect → TokenKind）
        ├── theme.rs         # 颜色
        └── wrap.rs          # 换行与光标映射
```

## 已知边界

- 只支持纯文本对话，没有图片输入、审批、MCP。
- Markdown 渲染是精简版：没有 OSC 8 超链接、表格的 key/value 降级、mermaid 和数学公式；图片只显示 alt 文本和地址。代码块有语法高亮（见"回答的 Markdown 渲染"）。
- 插件是编译期静态链接的：新增/修改插件需要改代码并重新编译，不支持运行时加载 `.so` 或脚本插件；插件与主程序同权限，没有沙箱隔离。
- 插件钩子在 TUI 线程同步执行，不能做阻塞 I/O。
- 会话按 rollout 保存在 `~/.codest/sessions/<工作区哈希>/`（`CODEST_HOME` 可覆盖），跨次启动可用 `--continue` / `--resume` 恢复；没有并发写锁（同时开多个 codest 写同一工作区没有保护）、没有归档/删除/搜索，也没有跨工作区的会话迁移。
- 没有网络重试、代理、认证刷新；HTTP 客户端只实现一次请求 + SSE 增量解析。API key 支持 `$VAR` 模板引用环境变量，但没有 OAuth/凭据存储，token 轮换需在协议工厂内部自行解决。
- 没有外网也能玩：`protocol = "test"` 提供进程内 echo 后端；真实端点仍需 API key 或先启动 `scripts/openai_test_server.py`。
- 思考内容（`reasoning_content`）以暗色渲染在回答上方，`/thinking` 随时折叠/展开；`/effort` 以 `reasoning_effort` 字段随请求发送（等级表取模型声明的 `reasoning`，未声明时用内置默认集）。模型元数据可带 `default_level`（providers.toml 声明或 models API 返回均可）：选中该模型时自动作为初始推理等级，footer 常显当前值。footer 常显上下文窗口与用量：有 `context_window` 时总长始终显示（测量前 `—/100.0k`，测量后 `13.0k/100.0k (6%)`，占用 ≥90% 变强调色；无窗口只显示 `13.0k tok`），`/model` 菜单每行列出各自的窗口长度，`/status` 输出用量、窗口与会话累计；models API 的 `context_length`/`context_window` 字段会自动填充 `ModelInfo::context_window`。每轮完成的模型与 usage 以 `turn_usage` 变体写入 rollout（后端未报 usage 的轮次不记录），`/clear` 一并重置；`--continue`/`--resume` 回放为初始统计（含最后使用的模型选择：provider 已不在配置中时保持当前选择并提示），恢复的会话立即显示上下文占用，会话间切换同样恢复该会话的模型（已在位时不重复切换）。思考内容仅用于显示：不进模型上下文、不写 rollout。
- 没有配置任何 provider 时以空路由启动：发消息得到添加指引（内容保留在输入框），不会发出请求。
- `fullscreen` 只有滚轮滚动，应用内鼠标选区/复制尚未实现；选区用终端原生能力（通常按住 `Shift` 拖拽）。
- `inline` 下已经写进 scrollback 的行不会随窗口宽度重新换行。
- 测试全部离线：`cargo test` 不会发起任何网络请求；`scripts/pty_smoke.py` 只访问本机启动的测试服务。

## 后续可扩展点

1. 在 `Op` 中加入工具调用与审批事件，向 codex 的 SQ/EQ 靠拢，并让插件可以注册工具。
2. 为 `Conversation` 增加 token 预算与压缩（消费 `ModelInfo::context_window` 与 `TurnUsage`）。
3. `/effort` 写回 `config.toml` 作为全局默认。
4. `/provider` 菜单里删除 provider（`ModelPool::remove_instance()` 已就绪，尚未接菜单）；以及保留注释的 `toml_edit` 写回。
5. ~~Markdown 渲染~~ 已实现（见"回答的 Markdown 渲染"）：`pulldown-cmark` 事件流 → ratatui 行；代码块语法高亮用 `syntect` + `two-face`，藏在 `highlight.rs` 的 `TokenKind` 接口后面。剩余项是表格的 key/value 降级和超链接（OSC 8）。
6. `fullscreen` 下的应用内鼠标拖选与 OSC 52 复制（对齐 pi/cht）。
7. 用 `inventory`/`linkme` 做分布式静态注册，保持 `Plugin` 契约不变（`cst-plugin-api` 拆分后只影响宿主侧收集方式）。
8. ~~统一本地插件安装方式~~ 已实现：`codest plugin install|uninstall|list|build`（本地路径 / git / crates.io，生成仓库外的 wrapper crate，不脏 git）。剩余项是等 `cst-plugin-api` 发布后去掉 `[patch.crates-io]`，以及可选的插件签名与版本约束求解。
