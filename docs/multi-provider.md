# codest 多 Provider 支持设计

> 状态：已实现（`src/core/providers.rs`、`src/core/config.rs`），`/model` 菜单、
> models API 发现、插件协议钩子均可用。
>
> 关键词：**kind/instance 分离、具名线协议、模型目录、回合边界切换、数据化配置**。

## 1. 目标

codest 原本只支持单一 OpenAI 兼容端点：client 在 `cli.rs` 由 flag/env 硬编码组装，
worker 启动时吃掉一个 `Arc<dyn ModelClient>` 直到退出。多 Provider 支持要解决的是：
**在不动 `ModelClient` 契约和 worker 循环骨架的前提下，让"新增一个端点"变成改配置、
让"新增一种线协议"变成写一个插件。**

| 目标 | 做法 |
| --- | --- |
| 一个 provider 挂多个模型 | 实例声明 `models` 数组；选择 id 统一为 `provider/model` |
| models API 发现 | 协议工厂可选实现 `list_models`；拉取结果与声明做增量合并 |
| 配置文件声明端点 | `~/.codest/providers.toml`（实例）+ `~/.codest/config.toml`（全局设置） |
| 运行时切换模型 | `Op::SwitchModel`，只在回合边界生效（op FIFO 保证） |
| 插件贡献自定义协议 | `Plugin::protocols()` 返回 `ProtocolFactory`（编译期）+ 实例（配置期） |
| 单一 `ModelClient` 缝隙 | 一切最终仍构造为 `Arc<dyn ModelClient>`；worker 只认这个 trait |
| 可测试 | 注册表、配置、目录合并、切换全部离线可测；`test` 协议内置 echo 后端 |

非目标（v1 明确不做）：

- usage 的 UI 渲染（数据通道已打通，见 §7；thinking 渲染与 `/effort`、
  `/thinking` 已落地——见 §7 与 `core::reasoning`）；
- OAuth / 凭据存储 / token 刷新（工厂的 `create()` 边界内自行解决）；
- 模型添加向导 / `$EDITOR` 编辑（手改 providers.toml）；
- 后台定时刷新、自适应探针、外部元数据源（models.dev 之类）——DSH 需要它们是因为
  长驻多进程宿主与免费通道，codest 是单进程交互式 TUI，菜单打开即现值；
- `context_window` 等模型元数据的消费（等 compaction 落地）。

## 2. kind / instance 分离

这是整个设计的核心，它解决了静态插件系统的 `&'static` 契约与运行期配置的冲突：

```text
ProtocolFactory（kind，编译期，'static）        ProviderInstance（instance，配置期，String）
┌──────────────────────────────┐              ┌─────────────────────────────────┐
│ id: "openai-compat" | "test" │   1 : N      │ name / protocol / base_url      │
│ display_name                 │ ◀──protocol──│ api_key($VAR) / headers         │
│ create(instance, model)      │              │ models[] / fetch_models         │
│ list_models(instance)?       │              │ body_overrides                  │
└──────────────────────────────┘              └─────────────────────────────────┘
        内置 2 个 + 插件贡献                          providers.toml 或 TUI 产生
```

- **协议种类**是 `ProtocolFactory`——一个 `'static` 值，知道一种线协议，能从实例配置
  构造 `Arc<dyn ModelClient>`。内置 `openai-compat`（`openai_compat.rs`）与 `test`
  （进程内 echo）；插件经 `Plugin::protocols()` 贡献更多。
- **实例**是 `ProviderInstance`——纯配置数据，来自 `providers.toml`（将来可由 TUI
  表单产生），用 `protocol` 字段按 id 引用一种协议。"自定义端点"不是一个特殊概念，
  只是一个 `base_url` 不同的 `openai-compat` 实例。
- **选择**是 `Selection`——`"provider/model"` 字符串，**在第一个 `/` 处切分**，因此
  模型 id 本身可以含 `/`（如 `gateway/deepseek/deepseek-r1`）。

这样划分后，auth 的任意复杂度（多源 token、401 热切换、签名）都被关进
`create(&self, instance, model)` 的边界里——core 永远不需要知道它们的存在，与
pi-ai 用 fetch wrapper 覆盖同类需求的经验一致。

## 3. 配置文件

两个文件，职责分离（都在 `codest_home()` 下，尊重 `CODEST_HOME`）：

```toml
# ~/.codest/config.toml —— 全局设置（本轮仅一项）
default = "openai/gpt-4o-mini"     # 初始 selection
```

```toml
# ~/.codest/providers.toml —— provider 实例
[[providers]]
name = "openai"                      # 唯一；不能含 '/'
protocol = "openai-compat"           # 缺省值
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"          # $VAR 模板；未解析按空 key 处理（回合报错，不阻塞启动）
fetch_models = true                  # 缺省规则：未声明 models 视为 true，已声明视为 false

  [[providers.models]]               # 显式声明（静态路径，优先）
  id = "gpt-4o-mini"
  name = "GPT-4o mini"               # context_window 驱动 footer / /status 的
  context_window = 128000            # 用量显示；reasoning / default_level 供 /effort 使用
  reasoning = ["low", "medium", "high"]
  default_level = "medium"           # 选中该模型时自动作为初始推理等级
                                     # 声明里缺的字段（如不写 context_window）会由
                                     # models API 的发现结果补齐，声明值优先

[[providers]]
name = "glm-local"
base_url = "http://127.0.0.1:8080/v1"
api_key = "test"
headers = { "X-Custom-Header" = "value" }      # 静态请求头：多数"特殊端点"免插件
body_overrides = { temperature = 0.5 }          # 请求体字段覆盖（覆盖标准字段）
```

规则：

- **没有内置默认 provider/base_url/model**:仅当 `providers.toml` 为空/缺失 **且**
  flag/env 提供了端点时才合成临时单端点实例。`base_url` 与 `api_key` **成对**要求:
  只给其一时实例照常创建,但发送消息与 `/model` 获取列表会被拦截,提示补全另一半
  (config 声明的实例不受此约束——免 key 端点是合法配置)。不指定 model 时实例
  不带模型列表,由 `/model` 从 `base_url` 的 models API 发现(启动即后台拉取,
  发现完成前发送消息提示先选模型);
- **都不可用 = 空路由**:注册表为空、无活动选择、无 client。发送消息时前端给出
  添加指引(`NO_MODEL_GUIDANCE`,输入内容保留);worker 侧同样有守卫,非 TUI
  前端也不会发出必败请求;`Op::SwitchModel` 可从空路由直接迁移到有路由;
- **解析错误启动即失败**，错误信息带文件路径与细节；未知字段同样报错（抓手误）；
- **优先级**：CLI flag > 环境变量 > config.toml `default` > 第一个实例的第一个模型。
  有 providers.toml 时 `--base-url` / `--api-key` 变为"覆盖所选实例字段"。

## 4. 模型目录与 models API

每个实例对外的模型列表 = **声明（静态）∪ 发现（增量）**：

1. 启动时从 `~/.codest/model-cache.json` 预载上次发现的结果——菜单即时可用，
   断网也不空转；
2. 对 `fetch_models` 生效的实例后台拉取（`openai-compat` 工厂实现
   `GET {base_url}/models`，OpenAI 形状 `{data:[{id}]}`；5s 超时）；
3. **合并规则：声明的排前且不被覆盖，发现结果只做增量**；
4. 发现完成/失败都只产生一条 transcript 通知（`Notice`），绝不报错打断会话；
5. 成功后写回缓存。

`/model` 菜单里按 `r` 可手动重跑当前选中项所属 provider 的发现。

## 5. 注册与校验

`ProviderRegistry::build(extra_factories, instances)` 是唯一的校验点（延续
`PluginHost::new` 的"构造时校验"哲学）：

- 实例 name 非空、唯一、不含 `/`；
- `protocol` 必须能解析到工厂（错误列出全部已知协议 id）；
- 声明的模型 id 非空、实例内唯一；
- 工厂 id（内置 + 插件贡献）不重复。

选择解析（`parse_selection`）的失败分两档：**未知 provider 是硬错误**（启动失败）；
未知 model 只告警——client 把 model 当纯字符串发送，是否有效由端点回答，且发现
可能稍后补全目录。

## 6. 运行时切换

worker 侧引入 `ModelBackend`：

```rust
enum ModelBackend {
    Fixed(Arc<dyn ModelClient>),                     // 测试与简化宿主用
    Pooled { pool, current: Selection, client },     // 多 provider,已有活动选择
    Empty { pool: Arc<ModelPool> },                  // 什么都没配置:空路由
}
```

- `Op::SwitchModel { selection }` 完全镜像 `Op::SwitchSession`：成功则换
  `current` + `client` 并发 `EventMsg::ModelSwitched`，失败（未知 provider、工厂
  报错）发 `EventMsg::Error` 且**保持现状**；**从 `Empty` 出发的首次成功切换会把
  状态迁移为 `Pooled`**；
- op 队列 FIFO 保证切换只在回合边界生效，绝不断流；
- **空路由的回合守卫**：`Op::UserTurn` 在 `Empty` 下直接回 `EventMsg::Error`
  （`NO_MODEL_GUIDANCE`），不构造请求；TUI 在提交入口先拦一步——给出同样的指引且
  **保留输入框内容**，两道防线共用一个文案常量；
- `ModelPool` 按 selection 懒建并缓存 client——切换是缓存命中，配置里再多的模型
  也不会预建连接；
- 切换与初始选择都记进 rollout：`SessionMeta.provider/model` 记初始值，
  `RolloutItem::ModelSwitched` 逐次追加。两者都带 serde default/skip——旧文件
  照常加载，旧版本读新文件天然跳过未知 item。空路由会话的 meta 不含
  provider/model 字段（`None` 跳过序列化）。

TUI 侧 `/model` 完全复用 `/sessions` picker 的模式（`MODEL_COMMAND` 进命令表、
`run_command` 拦截、模态 ↑↓/Enter/Esc/`r`、复用窗口渲染），Enter 乐观更新
footer 的 `provider/model` 标签，事件到达时权威回填。

## 7. 流层：StreamItem 与共享 SSE

为让多 provider 不锁死数据通道，两处基础设施先行：

- **`ModelStream` 条目从 `String` 改为 `#[non_exhaustive] enum StreamItem`**：
  `Delta`（正文）、`Thinking`（`delta.reasoning_content`，reasoning 模型的思考
  增量）、`Usage`（token 计数）。worker 把 Thinking 转成
  `EventMsg::AgentThinkingDelta`（TUI 以暗色渲染、`/thinking` 可折叠；仅显示，
  不进模型上下文与 rollout）、Usage 在完整结束时以
  `EventMsg::TurnUsage` 发出。渲染策略可以后补，通道先通——enum 后加 variant
  是破坏性变更，实现方会全体返工；
- **per-turn 请求选项**：`ModelClient::stream_chat` 增加 `TurnOptions`
  （当前含 `effort`，`/effort` → `Op::SetReasoningEffort` 设定、FIFO 生效），
  `openai-compat` 把它作为请求体的 `reasoning_effort` 字段发送——合并顺序为
  标准字段 < `body_overrides` < effort（交互式设置赢过静态覆盖）。
- **SSE 帧解析提取为 `core::sse`**：按规范实现（帧以空行结束、同帧多行 `data:`
  以 `\n` 连接、`event:` 字段、单空格剥离、CRLF/UTF-8 跨块容错）。`openai-compat`
  消费它；插件协议不应自带 SSE 解析，直接复用。

## 8. 插件协议（自定义 provider）

插件对多 Provider 的贡献点只有一种：**新的协议种类**。

```rust
struct VendorFactory;

impl ProtocolFactory for VendorFactory {
    fn id(&self) -> &'static str { "vendor-protocol" }
    fn display_name(&self) -> &'static str { "Vendor" }
    fn create(&self, instance: &ProviderInstance, model: &str)
        -> Result<Arc<dyn ModelClient>, ModelError> { /* 自定义线协议 */ }
    // 可选：models API 发现
    // fn list_models<'a>(&'a self, instance: &'a ProviderInstance, cancel) -> Option<...>
}

impl Plugin for Vendor {
    fn id(&self) -> &'static str { "vendor" }
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        static F: &[&'static dyn ProtocolFactory] = &[&VendorFactory];
        F
    }
}
```

两条扩展路径（同 pi 生态的经验）：

| 路径 | 何时用 | 成本 |
| --- | --- | --- |
| **只写配置**：`openai-compat` 实例 + `headers` / `body_overrides` | 端点说 OpenAI 方言，只是要加头/改体/换 URL | 零代码 |
| **插件协议**：`Plugin::protocols()` 贡献 `ProtocolFactory` | 请求体结构、流格式、签名机制不同 | 实现一个工厂 + client |

要诚实的边界：静态插件体系下，协议插件给的是**维护者的模块化**（vendor 协议代码
不进 core、可按 feature 裁剪），不是最终用户的可扩展性——第三方仍需重新编译。

不过“第三方要重新编译”已经变成**一条命令**，而不是“改宿主源码”：

- 契约类型（`ProtocolFactory`、`ModelClient`、`ProviderInstance`、`SseDecoder` …）
  拆到了独立的 `cst-plugin-api` crate。插件仓库依赖它，宿主依赖插件，因此没有
  Cargo 循环依赖（§8 的 `VendorFactory` 例子现在写在插件仓库里）；
- 宿主侧只需要 `codest plugin install <path|--git|name>`：它把插件记进
  `~/.codest/plugins.toml`，在 `~/.codest/plugins/crate/` 生成一个依赖宿主与插件
  的独立 crate，并在那里 `cargo build`。仓库本身零改动。详见
  `docs/local-plugins.md`。

`ProviderInstance` 为此新增了一个通用口袋 `[providers.options]`：插件私有配置原样
传给 `ProtocolFactory`，宿主从不解释任何键。守住的底线是——未知的**顶层**键仍然
是启动错误，配置文件继续是面向用户的扩展面。

运行时新增 provider 也已落地：`ModelPool` 的注册表由不可变字段改为
`RwLock<Arc<ProviderRegistry>>`，`install_instance()` 复用启动时的校验并逐出该
provider 的 client 缓存（旧 client 烘焙着旧 `base_url`/`api_key`）。`/provider`
向导是它的唯一调用方，见 `docs/provider.md`。

## 9. 测试策略

全部离线，与既有分层一致：

1. `core::config`：两文件解析、缺文件、解析错误带路径、`fetch_models` 缺省规则、
   `$VAR` 模板（字面量/未设/空值）、TOML→JSON 覆盖；
2. `core::providers`：Selection 切分（含模型 id 带 `/`）、注册表五类校验、默认选择、
   池的懒建与缓存、发现合并与 model-cache.json 落盘回载、OpenAI 目录解析去重；
3. `core::sse`：空行分帧、多行 data、CRLF、UTF-8 跨块、`finish` 容错；
4. `core::openai_compat`：content/reasoning_content/usage 解析、body 覆盖、空 key；
5. `core::session`：SwitchModel 成功/失败保持/固定后端拒绝/从空路由迁移、FIFO 边界、
   空路由回合得到指引、rollout 元数据落盘；
6. `tui::app`：`/model` 菜单开关、导航、切换、busy 拒绝、事件回填 label、渲染行、
   空路由提交（提示 + 输入保留 + effect 守卫）；
7. `scripts/pty_smoke.py`：每模式四组——flag/env 兼容路径 + providers.toml 全路径
   （发现通知、菜单列出 fetch 到的模型、切换、切后对话）+ 空路由路径（`no model`
   footer、指令照常、发送得指引、输入保留）+ 单边端点路径（提示补全另一半）。

```bash
cargo test
cargo clippy --all-targets -- -D warnings
python3 scripts/pty_smoke.py
```

## 10. 边界与演进

- client 不做重试、代理、认证刷新（与单端点时代一致）；`ModelError::NotConfigured`
  仍是"回合报错"而非启动失败，部分配置的 providers.toml 不影响其他实例；
- `/model` 添加端点的表单向导、`$EDITOR` 流程留待后续（先手改 providers.toml）；
- `Usage` 的消费已落地：footer 常显上下文窗口与用量（窗口总长始终显示，最后一轮
  `input + output` 对无状态 chat-completions 即下一轮请求的近似大小；测量前为
  `—/100.0k` 占位）、`/model` 菜单每行列出各自的窗口长度、`/status` 输出窗口
  占用与会话累计；每轮的模型与 usage 以 `turn_usage` 变体进 rollout，恢复时回放
  为初始统计并恢复最后使用的模型选择（`--continue` 启动即采用；会话间切换尽力
  恢复，选择已不存在时保持当前模型）；models API 的 `context_length` /
  `context_window` 字段自动填充 `ModelInfo::context_window`。按 `context_window`
  自动压缩仍是现成的下一步；thinking 的暗色渲染与折叠（`/thinking`）、推理等级
  （`/effort`）已落地；
- 想要 anthropic-messages 之类第二协议时，写一个内置工厂或示范插件即可，注册表
  无需改动。
