# Profiles（会话预设）

[English](../profiles.md) | 简体中文

Profile 是一份命名预设：以哪个模型启动、初始推理等级、保留哪些插件工具，
以及可选的系统提示词追加。定义在 `~/.codest/profiles.toml`，用
`--profile <name>`（环境变量 `CST_PROFILE`）启动，运行期用 `/profile`
切换。

## 1. 总览

```toml
# ~/.codest/profiles.toml
[profiles.code]
default = "openai/gpt-4o-mini"   # 初始选择
effort = "high"                  # 初始 /effort
tools = ["fs"]                   # 保留工具的插件 id

  [profiles.code.prompt]
  append = "You are a senior Rust engineer..."   # 追加到 system prompt 末尾

[profiles.fast]
default = "local/qwen2.5"
effort = "low"
```

Profile 为一次运行组合能力；它不声明任何端点——provider 的接线在
`providers.toml`（见 [multi-provider.md](../multi-provider.md)）。这些值是
**预设，不是第二配置层**：CLI flag 仍能覆盖其中任何一项，被恢复的会话
还原的是它自己的状态。

## 2. 配置参考

| 字段 | 类型 | 缺省 | 含义 |
| --- | --- | --- | --- |
| `default` | 字符串 | — | 初始模型选择，`"provider/model"`。 |
| `effort` | 字符串 | — | 初始推理等级，与 `/effort <level>` 完全同义；`"off"` 清除覆盖。 |
| `tools` | 字符串数组 | 缺省 = 保留全部插件工具 | 声明给模型的工具所属的插件 id；`[]` = 无工具。 |
| `[profiles.<name>.prompt]` `append` | 字符串 | — | 追加在基础提示词与插件片段之后——最具体的文本拿到最后发言权。 |

- 两级都从严解析（`deny_unknown_fields`）：profile 层与 `prompt` 层的
  拼错都是带文件路径的启动错误。
- 只有名字的 `[profiles.fast]` 合法——全缺省，纯粹一个可切换的标签。
- Profile 按确定（字母）顺序列出。
- 缺失的 `profiles.toml` 就是没有 profile。

### 带注解的示例

```toml
[profiles.code]
default = "glm/glm-5.3"        # 启动选择；赢过 config.toml 的 default，
                               # 输给 --provider/--model
effort = "high"                # 启动时等价于 /effort high
tools = ["bash", "fs"]         # 只有这些插件的工具声明给模型；
                               # 其余插件保留命令、钩子与提示词片段

  [profiles.code.prompt]
  append = "你是资深 Rust 工程师…"   # 追加在基础提示词与插件片段之后

[profiles.bare]                # 一个没有任何预设的命名标签

[profiles.chat]
default = "openai/gpt-4o-mini"
effort = "off"                 # 清除 effort 覆盖：不下发该字段
tools = []                     # 无工具——给不支持 function calling 的模型
```

## 3. 生命周期语义

四项绑定分成两半：

| 绑定 | 阶段 | 运行期可切换？ |
| --- | --- | --- |
| `default`、`effort` | 会话期 | 可以——`/profile <name>`，回合边界生效 |
| `tools`、`prompt.append` | 装配期（仅启动时） | 不可以——重启，或 `/new` 加新 profile |

- **装配期。** 启动时，活动 profile 的 `tools` 过滤作用到插件宿主（§5），
  `prompt.append` 拼接进系统提示词。两者在本次运行内固定；切换 profile
  不会重载它们。`/profile` 的帮助文本里也写明了这一点。
- **会话期。** `/profile <name>` 与 `/model` 一样提交 op，在下个回合边界
  生效，绝不打断流。切换先应用声明的选择（切换模型会用该模型的
  `default_level` 重播种 effort），再应用声明的 `effort`——后者有最终
  话语权。无法解析的选择以错误报告，但不中止其余预设——profile 仍然
  生效。`/profile` 不带参数列出可用 profile（`*` 标记当前，并显示各自的
  选择与 effort）。
- **持久化。** 启动 profile 记进会话元数据；每次切换向 rollout 追加一条
  `profile_switched` 记录。被恢复的会话还原**它自己**的状态——它最后
  使用的模型、推理 effort（`/effort` 或 profile 的 `effort`，显式 `off`
  也算）、它最后运行时的工具集、它最后使用的 profile 名（在 `/profile`
  里显示为活动）——而装配期绑定保持启动时的样子。从未选择过 effort 的
  会话，才由活动 profile 的 `effort` 播种（显式 `off` 也算数），否则由
  模型的 `default_level` 播种；记录过的选择绝不会被模型默认值覆盖。
- **CLI flag 永远赢。** `--provider`/`--model` 覆盖任何声明的 default；
  profile 是预设，不是对你 flag 的覆盖。

## 4. 选择优先级

一次运行以哪个模型启动：

| 优先级 | 来源 |
| --- | --- |
| 1 | `--provider` / `--model`（环境变量 `CST_PROVIDER` / `CST_MODEL`） |
| 2 | 活动 profile 的 `default` |
| 3 | `config.toml` 的 `default` |
| 4 | 第一个实例的第一个模型 |

- 只给 `--provider` 时以该 provider 第一个声明的模型启动；只给
  `--model` 时以第一个实例加该模型 id 启动。
- 未知 **provider** 是启动硬错误；未声明的**模型 id** 告警继续——模型
  是否存在由端点回答，发现也可能稍后补全目录。
- `--profile <name>` 名字不存在**不致命**：stderr 一条备注列出可用名字，
  运行继续，不带 profile。

## 5. 工具绑定

- `tools` 在装配期**按插件 id** 过滤：列出的插件保留工具声明与执行资格，
  其余插件的工具不下发模型。这不是卸载——被排除的插件保留命令、钩子与
  提示词片段（`/help`、`/clear` 等永远可用）。
- `tools` 缺省保留全部；空表 `[]` 剥掉所有工具（给不支持 function
  calling 的模型用）；列表里出现未知插件 id 是启动错误——拼错不能静默
  剥错工具。
- **与 `/tools` 的配合**：profile 在启动时按插件 id 粗筛；`/tools` 在
  会话期按工具细调。被 `/tools` 禁用的工具从请求中移除，执行期门禁兜底
  拒绝误调用，集合被记录（`tools_switched`）并按会话还原。组合使用：
  profile 里 `tools = ["fs"]`，会话内再 `/tools read off`，就是一个只读
  运行。

## 6. 推理等级（effort）

- profile 的 `effort` 与手敲 `/effort <value>` 完全同义：值原样应用，
  `"off"`（任意大小写）清除覆盖，一个 effort 字段都不发。
- 等级词汇来自**当前模型**：它声明的 `reasoning` 等级（从低到高，其
  `default_level` 即使不在列表里也始终可选），否则回落到内置的
  `["minimal", "low", "medium", "high"]`。`off` 不受列表约束，永远合法。
- profile 切换时先应用声明的选择——采纳该模型的 `default_level`——再
  应用 profile 的 `effort`，所以 profile 赢。
- 线上，effort 走实例的 `effort_field`（缺省 `reasoning_effort`；
  `"none"` 从不下发）——见 [multi-provider.md](../multi-provider.md) 的
  线协议 quirk 参考。

## 7. 设计笔记

- **预设，不是第二配置层。** 只有一条合并链——内置缺省 < 活动 profile
  < CLI flag——没有补丁层、没有按键覆盖、没有脚本。需要不同**线上**
  行为的场合，应写到 provider 实例上（`wire`、`headers`、
  `body_overrides`）：实例描述"怎么说话"，profile 描述"会话要什么"。
- Profile 刻意不触碰插件契约：工具过滤复用宿主现有过滤器，提示词追加
  在装配期拼接，没有新增任何钩子。
- 延伸背景：[plugins.md](../plugins.md)——静态插件系统；
  [multi-provider.md](../multi-provider.md)——provider 实例与协议。

## 8. 相关文档

| 文档 | 内容 |
| --- | --- |
| [multi-provider.md](../multi-provider.md) | Provider 实例、线协议 quirk、`/model`、`/provider`。 |
| [plugins.md](../plugins.md) | 静态插件系统；工具过滤器保留与舍弃了什么。 |
| [README.md](../../README.md) | CLI 参考与配置总览。 |
| [src/core/session.rs](../../src/core/session.rs) | `Op::SwitchProfile` 语义与 rollout 记录。 |
| [crates/cst-plugin-api/src/config.rs](../../crates/cst-plugin-api/src/config.rs) | `Profile` 字段定义。 |
