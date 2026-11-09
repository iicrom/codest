# 本地插件(第三方 provider 的接入方式)

> 状态:已实现。`src/plugins/local.rs` 是唯一的接入点,`crates/cst-plugin-api` 承载契约类型。
>
> 关键词:**编译期链接、契约 crate、宿主不依赖插件、一次 `cargo add` + 一行注册**。

## 1. 为什么仍然不是动态加载

`docs/plugins.md` §10 把"运行时加载 `.so`/`.dylib`"列为非目标,这条结论没有变。动态加载需要自建 C ABI、版本协商、流式回调与取消的跨 FFI 桥接,还要面对 panic 跨边界与二进制兼容问题;对当前规模的项目,收益远小于风险。

但原来的"必须改源码、和主程序一起编译"有一个真实缺陷:**第三方没有接入点**。本方案保留编译期链接,把它拆成两层:

```
codest ──depends on──▶ cst-provider-qoder ──depends on──▶ cst-plugin-api
   └─────────────────────── depends on ──────────────────────┘
```

- **`cst-plugin-api`**(本仓库 `crates/cst-plugin-api`):只有契约类型 `Plugin`、`ProtocolFactory`、`ModelClient`、`Message`、`ToolSpec`、`SseDecoder`、`AuthProvider` 等,没有 HTTP、没有终端、没有配置加载。它随 codest 一起发布,是插件的唯一依赖。
- **插件仓库**(例如 `cst-provider-qoder`):独立维护,依赖 `cst-plugin-api`,导出若干 `static` 值。
- **codest**:在 `Cargo.toml` 里依赖插件,在 `src/plugins/local.rs` 里登记一行。**codest 从不被插件依赖**,所以没有 Cargo 循环依赖。

宿主把 `cst-plugin-api` 的每个模块重新导出到原来的路径(`codest::core::model`、`codest::core::commands`、`codest::plugins::Plugin` …),因此这次拆分对既有代码与内置插件零破坏。

## 2. 三步接入

以 `../cst-provider-qoder` 为例:

```bash
cd ../cst

# 1. 加入依赖(会写 Cargo.toml 与 Cargo.lock)
cargo add --path ../cst-provider-qoder cst-provider-qoder
```

```rust
// 2. src/plugins/local.rs:取消那一行注释
pub static LOCAL_PLUGINS: &[&dyn Plugin] = &[&cst_provider_qoder::PLUGIN];
```

```bash
# 3. 编译
cargo build
```

`docs/provider.md` 接着讲 `/provider` 怎么用。

### 让 git 忽略这一行本地改动

`src/plugins/local.rs` 的用途就是"保持被修改",所以把它设为跳过工作区:

```bash
git update-index --skip-worktree src/plugins/local.rs
```

恢复跟踪(例如要升级本地插件列表):

```bash
git update-index --no-skip-worktree src/plugins/local.rs
```

## 3. 注册表语义

`PluginHost` 接收**分组切片**:`[BUILTIN_PLUGINS, LOCAL_PLUGINS]`,按组顺序、组内顺序拼接。由此得到:

- 插件 id、命令名/别名、工具名在**两组之间**统一校验,重复即启动失败——本地插件不能覆盖内置插件;
- 本地插件的 id 自动出现在 `plugin_ids` 里,所以 profile 的 `tools` 过滤、`/help`、`/status`、`/context` 都能看到它;
- `PluginHost::shipped()` 只返回内置插件,是测试与诊断用的稳定表(用户加了什么插件都不会让它变化)。

## 4. 一个外部插件能贡献什么

全部通过 `cst-plugin-api::Plugin` 的默认方法,只实现需要的部分:

| 扩展点 | 方法 | 典型用途 |
|---|---|---|
| provider 协议 | `protocols()` | 线协议与厂商 API 不同时实现 `ProtocolFactory`(Qoder 的 COSY 签名、置换 base64) |
| 登录入口 | `auth_providers()` / `auth_status()` / `begin_sign_in()` | 给 `/provider` 提供"登录到这个 provider"的能力 |
| 命令 | `commands()` / `run_command()` | 插件自己的斜杠命令 |
| 工具 | `tools()` / `run_tool()` | 提供给模型调用的工具 |
| 系统提示词 | `system_prompt()` | 注入一段上下文 |
| 生命周期 | `on_startup` / `on_user_message` / `on_turn_complete` / `on_conversation_cleared` | 观察与副作用 |

### 两条 provider 接入路径,先试便宜的那条

| 路径 | 何时用 | 成本 |
|---|---|---|
| **只写配置**:`openai-compat` 实例 + `headers` / `body_overrides` / `[providers.wire]` | 端点说 OpenAI 方言,只是要加头、改体、换 URL | 零代码 |
| **插件协议**:`protocols()` 贡献 `ProtocolFactory` | 请求体结构、流格式、签名机制不同(如 Qoder) | 一个工厂 + 一个 client |

先试配置路径。只有当协议本身不同时才值得写插件。

## 5. 契约版本

`cst-plugin-api::PLUGIN_API_VERSION` 当前是 `1`。

兼容规则:

- **加一个带默认实现的方法**:不升版本,插件无需改动;
- **改已有签名或语义**:升主版本,插件必须跟着更新;
- 宿主与插件通过 **Cargo 版本**达成一致,没有运行时握手:契约不兼容时插件直接编译失败,这比启动时才发现要好。

插件的 `Cargo.toml` 应写 `version = "0.1"` 这类范围,而不是精确版本。

## 6. 契约里有什么、没有什么

**有**(插件需要,宿主也需要的纯数据与 trait):

- `Message` / `Role`、`TurnOptions` / `StreamItem` / `ModelClient` / `ModelError`
- `ProviderInstance` / `ModelInfo` / `WireQuirks`(含 `[providers.options]` 口袋)
- `CommandSpec` / `CommandTable` / `Invocation`
- `ToolSpec` / `ToolCall` / `ToolCallAccumulator`
- `SseDecoder`(文档要求的"插件协议不应自带 SSE 解析")
- `Plugin` / `PluginEffect` / `PluginInfo` / `PluginContext`
- `AuthProvider` / `AuthMethod` / `AuthStatus` / `SignInFuture` / `SignInNotifier`

**没有**(留在宿主,插件不该碰):

- `providers.toml` 的读写、配置路径、`codest_home()`
- 会话、rollout、TUI、`ModelPool` 的注册与缓存
- 任何 HTTP 客户端或终端代码

`ProviderInstance` 里的 `[providers.options]` 是插件私有配置:宿主只解析成 `serde_json::Map` 原样传下去,从不解释任何键。因此插件加一个开关不需要宿主改 schema;未知**顶层**键仍然是启动错误。

## 7. 测试

```bash
cargo test --workspace              # 宿主 + 契约 crate,全部离线
cargo clippy --workspace --all-targets -- -D warnings
```

契约 crate 自己的测试覆盖那些"两边都必须一致"的细节:TOML 解析与 `$VAR` 模板、`options` 的类型读取、`[DONE]` 帧容错、SSE 跨块 UTF-8、工具调用分片重组。

## 8. 已知限制

- 本地插件与主程序**同生命周期**:改插件必须重新编译宿主。这是编译期链接的代价,不是 bug。
- 没有沙箱与崩溃隔离:插件 panic 会带下整个进程。
- 安装方式尚未统一:**目前是路径依赖 + 手动改一行**。`cargo install --git`、私有 registry 或一个 `cst plugin add` 子命令都是后续可选项,取决于第三方插件的实际数量。
