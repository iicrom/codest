# 配置

[English](../configuration.md) | 简体中文

codest 的状态全部在 `~/.codest` 下（根目录可用 `CODEST_HOME` 覆盖）。所有文件都是可选的；文件存在但解析失败——包括未知字段——会在启动时报错。密钥类取值支持 `$VAR` 模板，key 不必落盘。本页是文件与键位的参考；provider 实例与 profile 的深入内容以链接形式给出，不在此重复。

## 配置文件

| 路径 | 用途 |
| --- | --- |
| `config.toml` | 全局设置：`default = "provider/model"`、`max_tool_rounds`、`[status_bar]` 页脚表、按插件的 `[plugins."<id>"]` 配置口袋。 |
| `providers.toml` | Provider 实例（端点、密钥、模型）。 |
| `profiles.toml` | 命名会话预设。 |
| `plugins.toml` | 已安装的外部插件（机器管理）。 |
| `model-cache.json` | 发现结果缓存的模型列表。 |
| `sessions/<工作区哈希>/rollout-<uuid>.jsonl` | 会话 rollout（append-only JSONL）。 |

解析在所有文件上都是严格的（`deny_unknown_fields`）：写错的键是启动错误并指明文件，而不是静默失效的配置。`$VAR` 模板在使用时展开并去除空白；变量未设置或为空时解析为空值，导致该回合失败——而不是启动失败。

其中三个文件由机器管理，通常不需要手写：

| 文件 | 写入方 | 影响 |
| --- | --- | --- |
| `plugins.toml` | `codest plugin install` / `uninstall` | 保存外部插件列表，以及 wrapper crate 链接所用的 codest checkout。 |
| `model-cache.json` | 启动时的模型发现 | 预加载，`/model` 立即可用；刷新失败也不会清空菜单。每条记录带着发现的端点，只有端点仍与配置一致时才使用；已不再配置的 provider 在写回时被剪掉。旧版本 codest 留下的无标签缓存无法校验，会重新发现一次。 |
| `providers.toml` | `/provider` 向导（仅在确认时） | 向导写入是原子的，但格式由机器生成：手写注释与排版会丢失。 |

只有 `config.toml`、`providers.toml`、`profiles.toml` 供手写，且每个都是可选的——文件缺失就是什么都不贡献。

## `config.toml`

全局文件有两个顶层键加两张表：

| 键 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `default` | string | — | 初始模型选择，`"provider/model"`。 |
| `max_tool_rounds` | integer | 缺省 | 每个用户回合「模型 → 工具 → 模型」轮数的可选上限。 |
| `[status_bar]` | table | 分段开启 | 页脚：`plugin_segments` 总开关与插件分段的 `order` 显示优先级。 |
| `[plugins."<id>"]` | table | — | 插件私有的配置口袋；宿主不解释任何键。 |

```toml
# ~/.codest/config.toml
default = "openai/gpt-4o-mini"
# max_tool_rounds = 64

[status_bar]
plugin_segments = true                # 页脚显示插件分段（默认开启）
order = ["tps:rate", "tps:spark"]     # 显示优先级，从左到右

[plugins.tps]                         # 内置 tps 计量器的口袋
enabled = true                        # 启动即开启计量（默认关闭）
```

`max_tool_rounds` **默认不设上限**（与 codex 一致）：模型不再调工具、出错或按 `Esc` 才结束回合。设置数值即重新启用失控兜底——完成该轮数后回合以 `tool round limit reached` 错误结束。`64` 是常用的兜底值。

`default` 是启动选择链的第三环：`--provider`/`--model` > 激活 profile 的 `default` > `config.toml` 的 `default` > 第一个实例的第一个模型。它以 `"provider/model"` 指名某个实例的某个模型，在第一个 `/` 处解析，因此模型 id 本身可以带 `/`。

**`[status_bar]`** 只控制页脚的插件一侧——宿主自己的分段（模型、effort、上下文、状态）始终渲染。`order` 以显示优先级列出完整分段键（`plugin:key`）；未列出的分段保持注册顺序，当前没有东西在显示的键会被忽略，所以过期条目没有代价。窄终端上优先级最低的分段先丢弃。

**`[plugins."<id>"]`** 是按插件的配置口袋——整张表原样交给插件
（[plugins.md](plugins.md) 第 6 节），插件经 `PluginContext::plugin_settings()`
读取。命名未知插件 id 的表是**启动错误**（静默无操作会掩盖拼写错误），所以
卸载插件时请顺手删掉对应的表。

### `/settings`

`/settings` 就是编辑这个文件的界面：一张卡放宿主自己的选项（目前是
`[status_bar] plugin_segments`），每个声明了设置的插件各一张卡，每行显示已存
的值或默认值。`↑`/`↓` 走行，`Enter` 翻转开关 / 推进列表 / 打开单行编辑器，
`←`/`→` 前后步进列表（与开关），`Esc` 关闭；`/settings tps` 打开时直接聚焦该
卡。

```text
 Settings › tps                                            1/2
 codest  ·  tps
╭─ codest ─ 宿主设置 ──────────────────────────────────────╮
│ ❯ 插件状态分段                                     [x]   │
╰──────────────────────────────────────────────────────────╯
╭─ tps ─ 吞吐计量 ─────────────────────────────────────────╮
│   tokens/秒 计量器                                 [x]   │
╰──────────────────────────────────────────────────────────╯
```

每次编辑**立即写入**（没有「未保存草稿」这种状态），并且值只会落回它来自的那
张表：宿主行写进它在 `config.toml` 里的点分路径，插件行写进该插件的
`[plugins."<id>"]` 口袋。三点值得知道：

* **写入是校验过且原子的。** 重写后的文档先被解析回严格的 `GlobalConfig`；
  任何会被它拒绝的内容都会连同原因显示在提示行并拒绝写入，替换经由临时同级
  文件 rename 完成，所以失败永远不会留下写了一半的 `config.toml`。
* **注释与手工排版在第一次保存时丢失。** 此后文件由程序管理，与 `/provider`
  向导的写入规则一致。
* **清空文本行会删除该键**，恢复插件默认值；翻转开关显式写入 `true`/`false`。
  多数编辑立即作用于当前会话（通过 `Plugin::on_settings_changed` 通知插件）；
  声明为仅在启动时生效的行带 `⟲` 标记（帮助栏会补一句 `restart to apply`），
  只持久化、不即时应用——宿主会跳过它的 hook。

`/tps` 仍是同一个计量器的快捷键：它只翻转本次运行，从不改写文件。要记录选择，
用 `/settings`。

## Provider 实例

`providers.toml` 是可选的。文件缺失即没有实例；若此时 flag 或环境变量指名了端点，codest 为本次运行合成一个 `openai-compat` 实例；两者都没有则路由为空，TUI 引导你添加 provider。

```toml
# ~/.codest/providers.toml
[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "$OPENAI_API_KEY"        # $VAR 模板——key 只留在环境变量里
fetch_models = true                # 从 GET /models 发现模型列表

  [[providers.models]]             # 也可以显式声明（优先于发现结果）
  id = "gpt-4o-mini"

[[providers]]
name = "local"
base_url = "http://127.0.0.1:8080/v1"
api_key = "test"
headers = { "X-Custom" = "v" }     # 静态请求头 / 请求体覆盖：多数"特殊端点"免插件
body_overrides = { temperature = 0.5 }

  [providers.wire]                 # 字段拼写级 quirk
  thinking_fields = ["reasoning"]  # 思考文本字段（默认 reasoning_content）
  effort_field = "none"            # /effort 的请求字段名；"none" = 从不下发
```

字段全集见 [multi-provider.md](../multi-provider.md)：`[[providers]]` 与 `[[providers.models]]` 的每一个键、`$VAR` 模板、模型发现与 `/model` 菜单、`/provider` 向导，以及 `wire` quirks 表。

## Profiles

```toml
# ~/.codest/profiles.toml —— 命名预设：模型 + effort + 工具 + 提示词
[profiles.code]
default = "openai/gpt-4o-mini"     # 初始选择
effort = "high"                    # 初始 /effort（"off" = 不下发 effort 字段）
tools = ["fs"]                     # 插件 id；缺省 = 全部工具，空表 = 无工具

  [profiles.code.prompt]
  append = "你是资深 Rust 工程师…"   # 追加到 system prompt 末尾

[profiles.fast]
default = "local/qwen2.5"
effort = "low"
```

Profile 是**预设**而非第二配置层：CLI flag 仍然优先，工具与提示词属装配期（只在启动时生效）。启动选择优先级：`--provider`/`--model` > 激活 profile 的 `default` > `config.toml` 的 `default` > 第一个实例的第一个模型。

字段全集、生命周期语义（会话期与装配期）、工具绑定与 effort 处理见 [profiles.md](../profiles.md)。

## Session 头

按会话路由与缓存的端点认 `session-id` 头（codex 拼写）。`wire.session_header`（默认 `"session-id"`）按实例控制该头——`"none"` 关闭，自定义拼写按原样使用：

```toml
[[providers]]
name = "strict-endpoint"
base_url = "https://example.test/v1"
wire = { session_header = "none" }          # 该端点拒绝未知头

[[providers]]
name = "gateway"
base_url = "https://gateway.test/v1"
wire = { session_header = "x-session-id" }  # 改用网关自己的拼写
```

`[[providers.models]]` 里的 model 级 `session_header` 覆盖实例级设置，包括 `"none"`。该头在静态 `headers` 之后应用，因此运行时真实的会话 id 会覆盖手写的同名条目；发送的值是活动会话的 UUIDv7。

## 环境变量

| 变量 | 说明 |
| --- | --- |
| `CST_MODEL` | 启动时的模型 id（选择的 `model` 半边）。 |
| `CST_PROVIDER` | 启动时的 provider 实例。 |
| `CST_BASE_URL` | OpenAI 兼容 base URL 或完整 `/chat/completions` URL。会跑自己的 `untitled` 实例，而不是改写已配置实例。 |
| `CST_API_KEY` | 端点 API key；覆盖所选实例。 |
| `CST_PROFILE` | 以 `~/.codest/profiles.toml` 中的某个 profile 启动。 |
| `CST_TUI_MODE` | `fullscreen`（默认）或 `inline`。 |
| `CODEST_HOME` | 全部状态的根目录，替代 `~/.codest`。 |
| `CODEST_SRC` | `codest plugin install`/`build` 用来链接 wrapper crate 的 codest checkout。 |
| `OPENAI_API_KEY` | 使用合成的单端点实例时，作为 `CST_API_KEY` 的兜底。 |
| `OPENAI_BASE_URL` | `CST_BASE_URL` 的兜底，仅在 `providers.toml` 为空时生效——导出它不会改写已配置实例。 |

`CODEST_SRC` 在安装器中的查询顺序位于显式 `--codest-src` 之后、二进制编译源目录与 `plugins.toml` 记录的路径之前；每个候选都会被校验为 codest checkout，失效的候选降级到下一个。见 [local-plugins.md](../local-plugins.md)。

## 下一步

- [getting-started.md](getting-started.md) —— 安装、快速开始、完整 CLI 表。
- [multi-provider.md](../multi-provider.md) —— provider 实例、wire quirks、模型发现、`/provider` 向导。
- [profiles.md](../profiles.md) —— 预设、选择优先级、装配期与会话期值。
- [local-plugins.md](../local-plugins.md) —— 已安装插件与 wrapper crate。

[← 返回 README](../../README.md)
