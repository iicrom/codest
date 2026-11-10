# codest Profile 与 Provider 适配层设计

> 状态：**quirk 数据层已实现**（`src/core/config.rs::WireQuirks`、
> `src/core/openai_compat.rs`、`src/core/providers.rs`）；**profile 系统已
> 实现**（`profiles.toml` + `--profile` + `/profile`，见 §4）；**异步工具
> 执行已实现**（§8.3，`spawn_blocking` + 中断优先）；**动态脚本引擎
> （rhai）评估结论：暂缓不引入**。
>
> 关键词：**分层声明式数据、quirk 数据字段、profile 绑定能力、零新增依赖、低内存**。

## 1. 背景与问题

三个输入条件催生这份设计：

1. **不同模型/provider 需要特定的 prompt 和 tools**：有的端点不认识
   `reasoning_effort` 字段，有的模型的 delta 用别的字段名承载思考文本，
   有的场景希望 `/effort high` 之外还能成组地切换"模型 + 提示词 + 工具集"。
2. **dsh（DeepSeek Harness）的 profile 设计被验证实用**：值得蒸馏其思想。
3. **低内存占用是本项目的立身之本**：14 个直接依赖、全流式、`'static`
   注册表、无缓存历史（见 README 与 `docs/plugins.md` §2）；任何设计方案
   先过"依赖预算"这道闸。

本文回答三个问题：现有 tools 插件系统与 provider 架构缺什么（§2）；
dsh 的 profile 该借鉴什么、裁剪什么（§3–§4）；以及**为什么不引入 rhai
这类动态脚本系统**（§7）。

## 2. 现状评估

### 2.1 provider 架构：已自洽，不推翻

`ProtocolFactory`（编译期 `'static`，`src/core/providers.rs`）×
`ProviderInstance`（配置期，`~/.codest/providers.toml`）× `ModelPool`
（懒建缓存）的 kind/instance 分离已经覆盖"新增端点=改配置、新增协议=写
工厂"（见 `docs/multi-provider.md`）。统一缝隙是
`Arc<dyn ModelClient>`，worker 只认 `stream_chat`。

已有的 per-provider 差异点全部是**字段拼写级**，其中大半天然数据容忍：

| 差异点 | 现状 | 位置 |
| --- | --- | --- |
| usage 双拼写（`prompt_tokens`/`input_tokens`） | 代码已双兼容 | `openai_compat.rs::parse_usage` |
| context window 双拼写（`context_length`/`context_window`） | 代码已双兼容 | `providers.rs::models_from_openai_list` |
| 请求体怪癖（temperature、嵌套字段等） | `body_overrides` 数据逃生舱 | `config.rs` + `openai_compat.rs` |
| 请求头伪装/鉴权变体 | `headers` 数据 | `config.rs` |
| **思考文本 delta 字段名** | 硬编码 `reasoning_content` | `openai_compat.rs`（本次参数化） |
| **effort 请求字段名** | 硬编码 `reasoning_effort` | `openai_compat.rs`（本次参数化） |

结论：**不存在需要图灵完备脚本才能表达的 provider 差异**。剩下的硬编码
两处用"quirk 数据字段"参数化（§5，本次已实现），不需要更多机制。

### 2.2 tools 插件系统：雏形完整，缺配置面

声明（`Plugin::tools()`）、执行（`Plugin::run_tool`）、回合循环
（`MAX_TOOL_ROUNDS = 8`）、rollout 记录、流式 tool_calls 重组全部打通。
真实缺口按优先级：

1. ~~**无配置化启停**~~——已解决：`PluginHost::builtin_with_tools` 按插件
   id 过滤工具声明与执行资格，profile 的 `tools` 字段驱动（§4.1）。
2. ~~**执行同步阻塞**~~——已解决：`execute_tool_call` 把每次调用放上
   `tokio::task::spawn_blocking` 并以中断优先的 `select` 等待——慢工具不再
   占据 worker 任务，Esc 在工具执行期间即时生效；被放弃的调用在阻塞池上
   跑完但结果丢弃（`ToolExecutor` 契约注释已注明，§8.3）。
3. **声明是 `'static`**——不能运行时增删工具；只有当"接入 MCP/运行时
   装载工具"成为真实需求时才值得把 `ToolSpec` 去 `'static` 化
   （`Arc<[ToolSpec]>` 即可，内存代价可控）。当前不做。

### 2.3 现有契约的最大空档（与 profile 相关）

插件不能拦截/改写请求，prompt 注入是启动期静态拼接（`PluginHost::system_prompt`）。
profile 的 prompt 覆盖因此设计为**装配期参数**（§4），不动插件契约。

## 3. dsh profile 设计的借鉴与裁剪

dsh 的 profile 是一个目录：`package.json` 声明有序 **bundles** 列表 +
`cordis.patch.yml` **补丁层**。其核心思想恰好与代码无关：

> **bundles 决定"存在什么能力"，补丁决定"每个能力是什么值"；三层叠加
> （bundle → profile → home），每行 last-write-wins，纯数据、可 dump
> 预览、失败 bundle 跳过不炸启动。**

即使宿主本身在 JS 生态、bundle 是真代码，profile 层依然坚持纯 YAML——
**"分层声明式数据"就足以表达能力组合，这是这套设计最值得学的一点**。

| dsh 机制 | cst 裁剪结果 |
| --- | --- |
| 目录式 profile（npm 包 + node_modules） | ❌ 单文件 `profiles.toml`；cst 插件是编译期的，profile 不需要装包 |
| 三层补丁叠放（bundle/profile/home） | ✅ 思想保留为 `内置默认 < profile < CLI flag` 的合并链 |
| 按 id 定向补丁、last-write-wins | ✅ TOML 字段级覆盖天然如此 |
| `--dump-config` 离线预览 | △ `deny_unknown_fields` + 启动校验代替；启动错误即"dump" |
| HMR 热重载 | ❌ 单 worker 架构下不值得；profile 切换在回合边界生效 |
| `!!js` 表达式 | ❌ 纯数据，见 §7 |
| 失败 bundle 进 `skippedBundles` | ✅ 单个 profile 解析失败不应拖垮其它 profile 的可用性提示 |

## 4. Profile 系统（已实现）

### 4.1 文件与字段

`~/.codest/profiles.toml`，单文件多 profile（与 `config.toml`/
`providers.toml` 风格一致）：

```toml
[profiles.code]
default = "glm/glm-5.3"        # 初始 selection
effort = "high"                # 初始 /effort；"off" = 不下发 effort 字段
tools = ["prompt_debug"]       # 启用这些插件声明的工具（按插件 id）；
                               # 其余插件的工具不下发；缺省 = 全部；空表 = 无工具

[profiles.code.prompt]
append = "你是资深 Rust 工程师…"   # 追加到 system prompt 末尾

[profiles.fast]
default = "deepseek/deepseek-chat"
effort = "low"
```

- `deny_unknown_fields`，与现有两个配置文件同一纪律：拼错即启动错误。
- **`tools` 是按插件 id 的工具开关**，不是卸载插件：被排除插件只失去工具
  声明与执行资格（`PluginHost::builtin_with_tools`），其命令、钩子、
  prompt 片段照旧——`/help`、`/clear` 永远在。这是"无 function calling
  的模型剥掉工具"与"命令仍然可用"两个需求的交集。列表里出现未知插件
  id 是启动错误（拼错不能静默剥错工具）。
- **合并顺序**：内置默认 < 选中 profile < CLI flag。profile 是"一组
  预设值"，不是第二配置层——flag 仍能临时覆盖其中任何一项。
- **装配期生效**：`cli.rs` 读 profile → 影响 `PluginHost` 的工具过滤与
  system prompt 拼接 → 传给 `SessionBootstrap`。不动插件契约，不加钩子。

### 4.2 选择与切换（已实现）

- CLI：`cst --profile <name>`；`profiles.toml` 缺失或无此名 → 回落到
  现行为（等价于"无 profile"），并在启动 notice 里提示一次。
- TUI：`/profile` 无参数列出可用 profile（`*` 标记当前）；`/profile
  <name>` 提交 `Op::SwitchProfile`，在**回合边界**生效（与
  `Op::SwitchModel` 同一条 op FIFO 纪律）。切换效果 = 先应用其
  `default`（复用 `apply_model_switch`，故模型切换的 `default_level`
  重播种先发生），再应用其 `effort`（经 `normalize_effort`，`off` =
  清除）——profile 的 effort 有最终话语权；工具集与 prompt 属装配期，
  不随切换重载（写入 §9 非目标）。
- 持久化：rollout 记 `ProfileSwitched`，`SessionMeta.profile` 记启动
  profile；会话恢复时还原该会话最后使用的 profile 名
  （`LoadedSession.profile`，用于 `/profile` 列表标记）。

### 4.3 与 provider quirk 的关系

profile 只组合"选什么"，quirk 表达"线上怎么写"（§5）。profile 不新增
quirk 语法；若某端点既是特殊拼写又只在特定场景用，直接在该
`ProviderInstance` 上写 `[providers.wire]`——实例是全局的，profile 是
会话侧的组合，两者不混合。

## 5. Provider 适配阶梯与 quirk 数据层（已实现）

给一个陌生端点做适配，按阶梯逐级取用，**能用数据就不写代码**：

| 阶梯 | 机制 | 场景举例 |
| --- | --- | --- |
| 1 | `headers` | 伪装 UA、额外鉴权头 |
| 2 | `body_overrides` | 顶层/嵌套请求字段修正、`stream_options.include_usage` |
| 3 | `[providers.wire]` quirk 字段 | 思考 delta 字段名、effort 字段名、关闭 effort 下发 |
| 4 | 写一个 Rust `ProtocolFactory` | 全新线协议（如 anthropic-messages）、协议级状态机 |
| 5 | （明确不做）脚本引擎 | 见 §7 |

### 5.1 `WireQuirks`（本次实现）

`src/core/config.rs`：

```rust
#[serde(default, deny_unknown_fields)]
pub struct WireQuirks {
    /// delta 字段按序尝试为思考文本，首个命中生效；空列表 = 关闭 thinking 解析
    pub thinking_fields: Vec<String>,   // 默认 ["reasoning_content"]
    /// /effort 的请求字段名；"none" = 从不下发该字段
    pub effort_field: String,           // 默认 "reasoning_effort"
}
```

- `ProviderInstance.wire: Option<WireQuirks>`，TOML 形态
  `[providers.wire]`；`None` 时客户端用默认拼写，**零配置行为逐字节不变**。
- `OpenAiCompatClient::with_quirks` 在工厂 `build_client` 处烘焙（与
  `with_headers`/`with_body_overrides` 同一构造期模式）；`request_body`
  按字段名下发 effort，`parse_sse_frame` 按列表解析 thinking。
- 校验两级：TOML 层 `deny_unknown_fields` 拒绝拼错的键；registry 构建期
  `WireQuirks::validate` 拒绝空字段名（`RegistryError::InvalidWireQuirks`）。
- **词汇有意自由化**（字符串而非枚举）：provider 发明新拼写快于枚举演进，
  字段名是数据不是逻辑；文档只需登记已知值，不必看守门。

示例：

```toml
[[providers]]
name = "stiff-gateway"
base_url = "https://gw.example.com/v1"

[providers.wire]
thinking_fields = ["reasoning", "thinking"]   # 该网关的思考字段拼写
effort_field = "none"                          # 端点拒绝未知请求字段
```

已知值登记（约定俗成，非穷举）：思考 delta 字段——`reasoning_content`
（DeepSeek 系）、`reasoning`（部分 OpenRouter 上游）、`thinking`
（Anthropic 风格网关）；effort 字段——`reasoning_effort`（OpenAI 标准）、
`reasoning`（个别网关）、`none`。

## 6. "特定模型/provider 需要特定 prompt 和 tools" 的绑定

两层各管一半：

- **实例级（线格式）**：`wire` quirk + `body_overrides` + `headers`，
  已实现。模型怎么"说"由 provider 决定。
- **会话级（能力组合）**：profile 绑定 model + effort + tools + prompt
  追加（§4，已实现）。会话要什么由 profile 决定。
- **远期**：`ModelInfo.supports_tools: Option<bool>`——声明不支持
  function calling 的模型，回合循环自动剥离 `TurnOptions.tools`（数据已
  在 `TurnOptions` 通道上，实现点在 `session.rs::run_turn` 装配请求处）。
  在 MCP 或多工具插件落地、出现真实的小模型适配需求时再做。

## 7. 动态脚本引擎（rhai）评估：暂缓不引入

### 7.1 结论

**不引入 rhai（或 mlua/wasm 等任何脚本层），但不是永久否决**——把评估
依据与重启条件写在此处，供未来复核。

### 7.2 理由

1. **需求侧**：现有 per-provider 差异全部是字段拼写/映射级，quirk 数据
   字段已 100% 覆盖（§2.1、§5）；"需要图灵完备逻辑"的场景尚无一例。
   复杂行为的正确归宿是**编译期 Rust**（新 `ProtocolFactory`）——它有
   类型、有测试、有 review，脚本三者皆无。
2. **先例侧**：dsh 的实践恰是反证——宿主在 JS 生态、bundle 本身就是
   代码，profile/补丁层仍坚持纯数据 YAML，只用极有限的 `!!js` 表达式；
   "分层声明式数据"表达能力被验证足够（§3）。
3. **代价侧**：rhai 新增约 1.5–2MB 二进制与编译时间、一条依赖、一种新
   语言面；同步解释器在单 worker 的 async 循环里是阻塞点。与"零新增
   依赖 + 低内存"的立身哲学直接冲突（`docs/plugins.md` §1 目标表）。
4. **阶梯侧**：§5 的 1–4 级还没用尽。数据（quirk）与编译期扩展点
   （工厂）之间隔着整个"用户可表达"的空间——先把 `body_overrides`
   做到极致，也轮不到脚本。

### 7.3 重启条件（满足其一再评估）

1. 出现**真实用户**（非维护者）需要不重编译注入逻辑，且 §5 阶梯 1–4
   均无法表达——先考虑 JSON Patch / TOML 覆盖类结构化表达，再考虑脚本。
2. 工具生态需要沙箱执行第三方逻辑——那时应评估的是 wasm 隔离而非
   内嵌解释器；而 wasm 的运行时内存（数十 MB 量级）与本项目低内存目标
   冲突，等于宣告"低内存版 cst"不做这件事。
3. quirk 字段膨胀失控（>10 个字段仍不够用）——这通常是适配层设计错了
   的信号，先回头修抽象，而不是加脚本。

## 8. tools 插件系统演进路线

与 `docs/plugins.md` §11 对齐，profile 相关项已完成：

1. ~~**配置化启停**（§11.6）~~：`PluginHost::builtin_with_tools` 按插件
   id 过滤工具声明与执行；profile 的 `tools` 字段驱动（已实现）。
2. ~~**profile 系统**（本文 §4）~~：已实现（`--profile` / `/profile` /
   rollout `profile_switched`）。
3. ~~**异步工具执行**（§11.3）~~：已实现——`ToolExecutor` 契约保持同步，
   `execute_tool_call` 用 `spawn_blocking` 执行并以中断优先的 `select`
   等待；`PluginEffect::Task`（启动/命令 effect 的任务化）不在本次范围，
   等真实插件需要阻塞 I/O 时再做。
4. ~~**会话级工具开关 + 首批内置工具**~~：已实现。`/tools`（`Op::
   SetToolsEnabled`）按工具名细调当前会话的启用集：禁用的工具不再声明给
   模型（省 token、防幻觉调用），执行期门禁兜底拒绝误调用；开关随 rollout
   记录（`tools_switched`）并按会话恢复。同时落地 `ToolSpec.read_only`
   标记与内置 `bash` / `fs`（`read`/`write`/`edit`）插件。这是 readonly /
   normal profile 的会话侧落点：profile 按插件 id 粗筛（装配期，§4.1），
   `/tools` 按工具名细调（会话期），未来 profile 可以再叠加"只启用
   `read_only=true` 工具"的种子集。
5. **`ToolSpec` 去 `'static'` 化**：仅当 MCP/运行时装载成为需求；
   `Arc<[ToolSpec]>` + owned executor，内存代价可控。
6. **渲染扩展 / 分布式注册**：维持 `docs/plugins.md` §11 原顺序，不因
   本文提前。

## 9. 边界与非目标

- profile 切换**不重载**工具集与 system prompt（装配期决定）；需要
  "换脑子"的会话用 `/new` + 新 profile 重启。工具的运行时调整走
  `/tools`（§8.4，会话级、按工具名），两者互补。
- profile 是 run 作用域：会话间切换不还原各自的 profile 名
  （`LoadedSession.profile` 数据已就位，等有显示面时再接）；工具集
  **是**按会话还原的（`LoadedSession.tools`，恢复会话时随之生效）。
- quirk 字段不做 per-model 级覆盖（实例级已够；模型级等真实需求）。
- 不引入任何脚本引擎、不引入 wasm 运行时（§7）。
- 不做 profile 的 HMR / 交互式编辑器（手改 `profiles.toml`，同
  providers.toml 的纪律）。
