# 多 Provider 框架

[English](../multi-provider.md) | 简体中文

codest 可以同时对接任意多个模型端点：能配置什么、TUI 命令做什么、贡献者
如何新增一种线协议。

## 1. 总览

两个刻意分离的概念：

| 概念 | 是什么 | 在哪里 |
| --- | --- | --- |
| **协议种类（kind）** | 编译期 `ProtocolFactory`——一个 `'static` 值，知道一种线协议，能从实例配置构造客户端 | 代码：内置 `openai-compat` 与 `test`，加上插件贡献的工厂 |
| **Provider 实例（instance）** | 纯配置数据，按 `protocol` id 引用一种协议 | `~/.codest/providers.toml`（或 `/provider` 向导） |

- **"自定义端点"不是特殊概念**：它只是一个 `base_url` 不同的
  `openai-compat` 实例。
- **多数"特殊端点"完全不需要写代码**：`headers` 覆盖伪装与鉴权变体，
  `body_overrides` 修正请求体字段，`wire` quirk 重命名 Provider 会分歧的
  字段拼写（§5），真正不同的线协议由插件贡献一个 `ProtocolFactory`
  （§6）。一切最终都收在 `ModelClient` 这一个 trait 后面——worker 只认
  `stream_chat`，鉴权复杂度都关在 `create()` 边界里。
- **选择（selection）** 命名一个实例的一个模型：`"provider/model"`，在
  *第一个* `/` 处切分，因此模型 id 本身可以含 `/`（如
  `gateway/deepseek/deepseek-r1`）。

## 2. 配置参考

所有文件都在 `~/.codest` 下（用 `CODEST_HOME` 改根目录）。每个文件都是
可选的；**存在但解析失败——包括未知字段**（`deny_unknown_fields`）——是
带文件路径的启动错误。全局设置在 `config.toml`（`default`、
`max_tool_rounds`），下文的实例在 `providers.toml`；`model-cache.json`
由机器管理。

### 2.1 `[[providers]]`

| 字段 | 类型 | 缺省 | 含义 |
| --- | --- | --- | --- |
| `name` | 字符串 | 必填 | 实例唯一名；也是选择里 `provider` 那一半。不能含 `/`、不能重复。 |
| `protocol` | 字符串 | `"openai-compat"` | 协议种类 id。内置：`openai-compat`、`test`；插件可贡献更多。 |
| `base_url` | 字符串 | — | API 根。请求走 `{base_url}/chat/completions`，发现走 `{base_url}/models`。 |
| `api_key` | 字符串 | — | 明文密钥或 `$VAR` 模板（2.3）。未解析只报回合错误，不影响启动。 |
| `models` | 表数组 | `[]` | 显式模型声明（2.2）。 |
| `fetch_models` | 布尔 | 未声明 `models` 时为 `true`，否则 `false` | 从 models API 发现模型列表（§3）。 |
| `headers` | 表 | `{}` | 随每个请求发送的静态请求头。 |
| `body_overrides` | 表 | `{}` | 合并进请求体的 JSON 字段；覆盖标准字段。 |
| `wire` | 表 | — | `openai-compat` 线协议 quirk（§5）。 |
| `options` | 表 | `{}` | 协议私有口袋，原样交给工厂（`option_str` / `option_u64` / `option_bool`）。未知*顶层*键仍被拒绝。 |

### 2.2 `[[providers.models]]`

| 字段 | 类型 | 缺省 | 含义 |
| --- | --- | --- | --- |
| `id` | 字符串 | 必填 | 发给端点的模型 id；实例内唯一、非空。 |
| `name` | 字符串 | — | 展示名（菜单、状态）。 |
| `context_window` | 整数 | — | 仅供参考：footer 用量显示与 `/model` 行。 |
| `reasoning` | 字符串数组 | — | 模型接受的推理等级，**从低到高**；供 `/effort` 使用（§5）。 |
| `default_level` | 字符串 | — | 选中该模型时采用的推理等级，直到 `/effort` 覆盖。 |
| `session_header` | 字符串 | — | 对该模型覆盖 `wire.session_header`；`"none"` 表示这个模型不发会话头。 |

声明条目优先于发现到的同名模型，但声明缺着的字段由发现补齐——只写
`id` 的稀疏条目也能拿到 models API 报告的上下文窗口。

### 2.3 `$VAR` 密钥模板

以 `$` 开头的值是环境变量名，使用时展开并去除首尾空白；字面量原样通过。
未设置或空变量解析为空——回合随即报错并给出指引；启动照常。

### 2.4 命令行、环境变量与启动规则

Flag 与环境变量：`--provider` / `CST_PROVIDER`（启动实例）、
`--model` / `CST_MODEL`（模型 id）、`--base-url` / `CST_BASE_URL`
（OpenAI 兼容 base URL，或完整 `/chat/completions` URL）、
`--api-key` / `CST_API_KEY`（在 `--help` 中隐藏；`OPENAI_API_KEY` 与
`OPENAI_BASE_URL` 是进一步兜底）。

- **没有内置默认 provider。** 仅当 `providers.toml` 为空/缺失 **且**
  flag/env 确实提供了端点时，才合成一个 `openai-compat` 实例（名为
  `untitled`）；什么都没有就是空路由，TUI 照常启动并指引添加 provider。
- `--base-url` 与 `--api-key` **成对**：只给其一时合成一个*不完整*实例，
  首次使用时被拦截并提示补全缺的另一半，而不是发出必败请求。
- 合成实例从不发明模型 id：不给 `--model` 时列表来自发现，由 `/model`
  挑选；给了 `--model` 则初始选择已定，但发现仍会填充菜单。有
  `providers.toml` 时，flag 改为**覆盖所选实例**。
- 校验从严：未知字段、重复的 name、含 `/` 的 name、未知 `protocol` id、
  空或重复的模型 id 都是启动错误（协议错误会列出全部已知 id）。选择里
  的未知 **provider** 是硬错误；未声明的**模型 id** 告警继续——模型是否
  存在由端点回答。

### 2.5 带注解的示例

```toml
# ~/.codest/providers.toml
[[providers]]
name = "openai"                        # 唯一、无 '/'——"openai/gpt-4o-mini" 的 provider 半边
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"            # $VAR 模板：密钥留在环境变量里
fetch_models = true                    # 列表来自 GET /models
  [[providers.models]]                 # 显式声明优先……
  id = "gpt-4o-mini"
  context_window = 128000              # footer 用量 + /model 行
  reasoning = ["low", "medium", "high"]  # /effort 词汇，从低到高
  default_level = "medium"             # 选中即采用，直到 /effort 覆盖

[[providers]]
name = "local"                         # 免 key 的本地服务器不需要插件
base_url = "http://127.0.0.1:8080/v1"
headers = { "X-Custom-Header" = "value" }      # 静态请求头
body_overrides = { temperature = 0.5 }          # 合并进请求体
wire = { effort_field = "none" }                # 端点拒绝未知请求字段
  [[providers.models]]
  id = "qwen2.5"
  session_header = "none"                       # 模型级覆盖：这个模型不发会话头
```

## 3. 模型发现与 `/model` 菜单

- 启动时从 `~/.codest/model-cache.json` 预载上次发现的结果，菜单即时
  可用，一次失败的刷新也不会清空它。`fetch_models` 生效的实例随后后台
  拉取：`GET {base[url}/models`（OpenAI 形状 `{data: [{id, ...}]}`），跳过
  没有 id 的条目、折叠重复、5 秒超时。成功与失败各产生一条 transcript
  通知——绝不致命——成功后写回缓存。上下文窗口的两种实际拼写都接受：
  `context_length`（OpenRouter）与 `context_window`。
- 每个实例的合并视图：声明的模型在前（声明顺序），其后是配置未声明的
  发现结果；声明条目保持优先，缺着的字段由发现补齐。
- `/model` 打开选择器，每个 `provider/model` 选择一行——同一实例的模型
  连在一起，按声明顺序——每行显示上下文窗口（已知时）与当前模型的
  标记。按键：`↑`/`↓` 移动、`Enter` 切换、`r` 重跑所选项所属 provider 的
  发现、`Esc` 关闭。切换在**回合边界**生效（op 队列 FIFO，绝不打断
  流）：footer 标签乐观更新，worker 的 `ModelSwitched` 事件是权威确认，
  失败的切换保持现状。客户端每回合现建、不缓存：切换只是改变下一回合
  客户端读到什么，会话 id 也从不烘进客户端。

## 4. `/provider` 向导

`/provider` 打开一个含三条路径的向导：

1. **添加 OpenAI 兼容端点。** 名称 → base URL → API key（`$VAR` 模板或
   明文；留空表示免 key）→ 模型 id（逗号分隔；留空表示自动发现）→ 后台
   对 `GET /models` 探测一次（失败不致命，在确认页显示为备注）→ 确认。
   实例写入 `providers.toml` 并安装进注册表，立即可选。
2. **登录。** 仅当插件贡献了登录入口时出现。选择目标（一个使用该插件
   协议的已配置实例，或插件的默认路由）与方式——浏览器授权或密钥输入
   （如 PAT）；`Esc` 取消。尚未配置的路由在成功后按插件默认值安装；凭据
   存在插件自己的存储里。
3. **已配置的 provider。** 只读列表：名称、协议、base URL、模型数、
   登录状态徽标。

写回：新的实例集合按与启动完全相同的规则校验，然后**原子**写入（临时
兄弟文件 + 改名）。一个后果：**注释与排版会丢失**——没有引入
`toml_edit`，第一次向导写入之后文件就是机器排版。手工排版的文件请继续
手工编辑；向导只在你确认时才写。

## 5. 线协议 quirk 参考

`[providers.wire]` 只覆盖三个没有安全兜底的线格式分歧点；其余一切
（usage 的 token 拼写、上下文窗口拼写、请求体修正）要么代码里已兼容，
要么用 `body_overrides` 表达。

| Quirk | 缺省 | 含义 | 关闭方式 |
| --- | --- | --- | --- |
| `thinking[fields` | `["reasoning_content"]` | 按序尝试的思考文本 delta 字段；首个命中生效。 | 空列表关闭 thinking 解析。 |
| `effort_field` | `"reasoning_effort"` | 携带 `/effort` 值的顶层请求字段。 | `"none"` 从不下发该字段。 |
| `session_header` | `"session-id"` | 携带会话 id 的请求头（值是当前会话的 UUIDv7，逐轮传入）。 | `"none"` 不发该头。 |

- 模型自己的 `models.session_header` 覆盖实例设置；任一层写 `"none"` 都
  会关掉该头（按模型，或按实例）。
- 字段名是**数据而非逻辑**：任何拼写都允许，这正是这一层的用途；只有
  空字段名会在启动时被拒绝。
- 线上合并顺序：标准字段 < `body_overrides` < 逐轮 effort——交互式的
  `/effort` 在这一个字段上赢过静态 `body_overrides`。会话头在 `headers`
  之后应用，活的会话 id 赢过手写的同名条目。

### 与 `/effort` 的配合

- `/effort [level]` 设置本会话的推理等级，按实例的 `effort_field` 下发。
  `off` 永远合法，清除覆盖（一个字段都不发）。端点拒绝未知请求字段时：
  设 `effort_field = "none"`（若它还容不下你不需要的 thinking delta，
  可再配 `thinking[fields = []`）。
- 等级词汇来自**当前模型**：它声明的 `reasoning` 列表（其 `default_level`
  即使不在列表里也始终可选），否则回落到内置的
  `["minimal", "low", "medium", "high"]`。选中一个模型即采用其
  `default_level`，直到 `/effort`（或 profile，见
  [profiles.md](../profiles.md)）覆盖。

## 6. 经插件自定义线协议

内置两种：`openai-compat` 与 `test`——进程内 echo 后端，完全离线，适合
演示与测试（`protocol = "test"`）。插件经 `Plugin::protocols()` 贡献新的
协议种类：

```rust
struct VendorFactory;

impl ProtocolFactory for VendorFactory {
    fn id(&self) -> &'static str { "vendor-protocol" }
    fn display_name(&self) -> &'static str { "Vendor" }

    // 实例状态（URL、密钥、headers、options）都经 `instance` 进入；
    // 鉴权、签名、重试都关在这条边界后。
    fn create(&self, instance: &ProviderInstance, model: &str)
        -> Result<Arc<dyn ModelClient>, ModelError> { /* 构造客户端 */ }

    // 可选：模型发现（openai-compat 即 GET /models）
    // fn list_models<'a>(&'a self, instance: &'a ProviderInstance, cancel)
    //     -> Option<ListModelsFuture<'a>>
}

impl Plugin for Vendor {
    fn id(&self) -> &'static str { "vendor" }
    fn protocols(&self) -> &'static [&'static dyn ProtocolFactory] {
        static F: &[&'static dyn ProtocolFactory] = &[&VendorFactory];
        F
    }
}
```

它的实例随后就是普通 TOML：

```toml
[[providers]]
name = "vendor"
protocol = "vendor-protocol"     # 对内置 + 插件工厂解析
[providers.options]              # 协议私有口袋
endpoint = "https://vendor.example/api"   # 用 option_str 读
max_output_tokens = 131072       # option_u64；还有 option_bool
```

契约类型（`ProtocolFactory`、`ModelClient`、`ProviderInstance`、SSE
解码器）在独立的 `crates/cst-plugin-api` crate 里，插件依赖契约而非宿主
二进制。要诚实的边界：协议插件给的是**维护者的模块化**——vendor 代码
不进 core、可按 feature 裁剪——不是最终用户的运行时扩展性。第三方确实
要重新编译，但那是**一条命令**（`codest plugin install …`），不是改宿主
补丁；注册表与池的内部实现见 `src/core/providers.rs`。延伸阅读：
[plugins.md](../plugins.md) / [local-plugins.md](../local-plugins.md)。

## 7. 设计笔记

- **为什么没有默认 provider**：默认 `api.openai.com` 或兜底模型 id，等于
  把你的密钥和对话送到一个你没有选过的地方；会自我解释的空路由代价更低。
  合成 `untitled` 实例的前提，是 flag/env *显式*给出了端点。
- **为什么 kind/instance 分离**：`'static` 插件契约带不了运行期配置，
  而 per-vendor 鉴权不能泄漏进 core。编译期工厂 × 配置数据实例把两头都
  钉死：一个工厂服务它的协议的所有实例，全部实例状态都经 `create()`
  进入。延伸阅读（设计笔记，中文）：[provider.md](../provider.md)——
  `/provider` 向导；[session-id.md](../session-id.md)——会话 id 与其
  请求头。

## 8. 相关文档

| 文档 | 内容 |
| --- | --- |
| [profiles.md](../profiles.md) | 模型 + 推理等级 + 工具 + 提示词的命名预设。 |
| [plugins.md](../plugins.md) | 静态插件系统及其契约。 |
| [local-plugins.md](../local-plugins.md) | `codest plugin install` 与第三方 provider 接入。 |
| [README.md](../../README.md) | 安装、CLI 参考、配置总览。 |
