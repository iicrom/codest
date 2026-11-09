# 本地插件(第三方 provider 的接入方式)

> 状态:已实现。`codest plugin install|uninstall|list|build` 是唯一入口,`crates/cst-plugin-api` 承载契约类型。
>
> 关键词:**编译期链接、契约 crate、仓库外的生成 crate、一条命令装完且不脏 git**。

## 1. 为什么仍然不是动态加载

`docs/plugins.md` §10 把"运行时加载 `.so`/`.dylib`"列为非目标,这条结论没有变。动态加载需要自建 C ABI、版本协商、流式回调与取消的跨 FFI 桥接,还要面对 panic 跨边界与二进制兼容问题;对当前规模的项目,收益远小于风险。

但"改宿主源码才能加插件"有真实缺陷:**接入点住在被 git 跟踪的文件里**。旧方案要 `cargo add --path`(改 `Cargo.toml`,写 `Cargo.lock`)再取消 `src/plugins/local.rs` 里一行注释,于是文档只能建议 `git update-index --skip-worktree`。那是个会反噬的技巧:文件一旦被跳过,升级本地插件列表时很容易忘掉,`git stash`/`checkout` 也会出错。

现在把"哪些插件被链接"从源码里挪出去。插件仍是编译期链接,所以必须**某处**的代码引用 `&PLUGIN` 静态值——但那份代码不必住在仓库里:

```
codest ──depends on──▶ cst-provider-qoder ──depends on──▶ cst-plugin-api
   ▲                                                          ▲
   └──────── ~/.codest/plugins/crate 依赖宿主 + 插件 ──────────┘
```

三个角色:

- **`cst-plugin-api`**(本仓库 `crates/cst-plugin-api`):只有契约类型 `Plugin`、`ProtocolFactory`、`ModelClient`、`Message`、`ToolSpec`、`SseDecoder`、`AuthProvider` 等,没有 HTTP、没有终端、没有配置加载。它随 codest 一起发布,是插件的唯一依赖。
- **插件仓库**(例如 `cst-provider-qoder`):独立维护,依赖 `cst-plugin-api`,导出若干 `static`。
- **宿主**:`PluginHost::with_local(local, filter)` 是注入点,`codest::cli::run_with` / `run_blocking` 把它接进启动流程。**codest 从不被插件依赖**,所以没有 Cargo 循环依赖。

宿主把 `cst-plugin-api` 的每个模块重新导出到原来的路径(`codest::core::model`、`codest::core::commands`、`codest::plugins::Plugin` …),因此这次拆分对既有代码与内置插件零破坏。

## 2. 安装:一条命令

```bash
cd ../cst
cargo build                                  # 只需要本体,不需要动 Cargo.toml
./target/debug/codest plugin install ../cst-provider-qoder
```

`plugin install` 做四件事:

1. 读插件清单,拿到**包名**、**crate 标识**和**导出符号**(默认 `PLUGIN`,可用 `[package.metadata.codest] plugins` 或 `--symbol` 覆盖);
2. 把条目写进 `~/.codest/plugins.toml`(原子替换);
3. 生成 `~/.codest/plugins/crate/`(独立的 Cargo 包);
4. 在那里 `cargo build`,打印产出的二进制路径。

产物:

```text
~/.codest/plugins.toml               已装插件列表(机器管理)
~/.codest/plugins/crate/
    Cargo.toml                       codest + 每个插件一行依赖
    src/main.rs                      static LOCAL_PLUGINS: &[&dyn Plugin] = &[&…::PLUGIN];
    src/bin/cst.rs                   同一个列表的 cst 别名
    target/debug/codest              带插件的二进制
```

其余子命令:

| 命令 | 作用 |
|---|---|
| `codest plugin list` | 列出已装插件、来源与符号,以及链接用的宿主路径 |
| `codest plugin uninstall <name>` | 从列表删掉并重建(插件没了,其他插件还在) |
| `codest plugin build [--release]` | 只重建(改了插件源码、或换了宿主路径) |
| `codest plugin build --install` | 用 `cargo install` 覆盖已安装的 `codest`/`cst` |

`--no-build` 只重写列表与生成物,不跑 cargo(适合脚本里批量改)。

### 三种来源

```bash
codest plugin install ../my-plugin                            # 本地 checkout(未知目录也会当作 path)
codest plugin install my-plugin --git https://host/r.git --tag v1
codest plugin install my-plugin --version '^0.1'               # crates.io
```

git 源可加 `--rev` / `--branch` / `--tag`(互斥);`--package` 用于包名无法从 URL 推断时;`--crate-name` 覆盖生成的 Rust 标识。

### git / crates.io 来源的前提

这两条路径要求插件**按版本**依赖契约:

```toml
cst-plugin-api = "0.1"     # 可以 clone 下来独立构建
```

而不是:

```toml
cst-plugin-api = { path = "../cst/crates/cst-plugin-api" }   # 只在本地并列目录里成立
```

第二种是本地开发的自然写法(插件仓库自己 `cargo test` 需要它),但克隆到别处后那个相对路径就不存在了;`[patch.crates-io]` 也救不了,因为它是 `path` 依赖而不是 registry 依赖。所以:

- **本地并列目录里开发** → 用 `path` 依赖,`codest plugin install <path>` 安装;
- **发布给别人从 git/crates.io 装** → 用 `version` 依赖(需要 `cst-plugin-api` 已发布),安装器会自动补 `[patch.crates-io]` 让两边的契约同源。

装错来源时 cargo 会报 `no matching package named cst-plugin-api found`,安装器在此之上补一行提示,并**不改动** `plugins.toml`。

### `--codest-src`:wrapper 链接哪个宿主

生成的 crate 必须 `path` 依赖一个 codest 源码树。解析顺序:

1. `--codest-src`;
2. `$CODEST_SRC`;
3. **编译该二进制时的源码目录**(`env!("CARGO_MANIFEST_DIR")` 固化进二进制);
4. `plugins.toml` 里上次记录的值。

每个候选都验证过(`Cargo.toml` 里 `package.name == "codest"` 且存在 `crates/cst-plugin-api`),不合格就降级到下一个。所以用 `cargo install` 装好的二进制仍能安装插件——它记着安装时的源码路径;源码被挪走时,显式传 `--codest-src` 即可。

### 为什么路径来源不需要 `[patch.crates-io]`

契约一致性是这套方案唯一的硬约束:宿主与插件必须解析到**同一个** `cst-plugin-api`,否则 `&QoderPlugin` 不会被当成 `&dyn Plugin`(编译期就报错,不会到运行时才炸)。

- 插件用 `path` 依赖契约 → 安装器把它解析成绝对路径,并**校验**它就是宿主那份(`ContractMismatch` 直接拒绝)。三方自动统一,无需 patch。
- 插件用 version/git 依赖契约 → 会解析成第二份 crate,于是生成物带:

  ```toml
  [patch.crates-io]
  cst-plugin-api = { path = "<codest-src>/crates/cst-plugin-api" }
  ```

Cargo 允许 patch 后未被使用(比如全是 path 来源),但会打印警告,所以只在需要时才生成。

## 3. 生成物长什么样

```toml
# ~/.codest/plugins/crate/Cargo.toml(开头是 GENERATED 注释)
[package]
name = "codest-local"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]                      # 自成 workspace 根:不被 $CODEST_HOME 的祖先吸走

[[bin]]
name = "codest"
path = "src/main.rs"

[[bin]]
name = "cst"
path = "src/bin/cst.rs"

[dependencies]
codest = { path = "/Volumes/Untitled/Projects/cst" }
cst-provider-qoder = { path = "/Volumes/Untitled/Projects/cst-provider-qoder" }
```

```rust
// ~/.codest/plugins/crate/src/main.rs
use codest::plugins::Plugin;

static LOCAL_PLUGINS: &[&dyn Plugin] = &[
    &cst_provider_qoder::PLUGIN,
];

fn main() {
    codest::cli::run_blocking("codest", LOCAL_PLUGINS);
}
```

`run_blocking` 让生成物不需要 `tokio`/`anyhow` 依赖:它自建 runtime、跑 `run_with`、出错时打印并 `exit(1)`。别名 `cst` 只差第一个参数。

**不要手改生成物**:每次 install/uninstall/build 都会整体重写。要改就改 `~/.codest/plugins.toml` 再 `codest plugin build`。

## 4. 注册表语义

`PluginHost::with_local` 把内置组与注入组拼在一起(`[BUILTIN_PLUGINS, local]`),按组顺序、组内顺序拼接。由此得到:

- 插件 id、命令名/别名、工具名在**两组之间**统一校验,重复即启动失败——本地插件不能覆盖内置插件;
- 本地插件的 id 自动出现在 `plugin_ids` 里,所以 profile 的 `tools` 过滤、`/help`、`/status`、`/context` 都能看到它;
- `PluginHost::shipped()` 只返回内置插件,是测试与诊断用的稳定表(装了什么插件都不会让它变化);
- 不带 wrapper 的普通 `cargo build` 产物传空切片,行为与之前完全一致。

## 5. 一个外部插件能贡献什么

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

## 6. 契约版本

`cst-plugin-api::PLUGIN_API_VERSION` 当前是 `1`。

兼容规则:

- **加一个带默认实现的方法**:不升版本,插件无需改动;
- **改已有签名或语义**:升主版本,插件必须跟着更新;
- 宿主与插件通过 **Cargo 版本**达成一致,没有运行时握手:契约不兼容时插件直接编译失败,这比启动时才发现要好。

插件的 `Cargo.toml` 应写 `version = "0.1"` 这类范围,而不是精确版本。

## 7. 契约里有什么、没有什么

**有**(插件需要,宿主也需要的纯数据与 trait):

- `Message` / `Role`、`TurnOptions` / `StreamItem` / `ModelClient` / `ModelError`
- `ProviderInstance` / `ModelInfo` / `WireQuirks`(含 `[providers.options]` 口袋)
- `CommandSpec` / `CommandTable` / `Invocation`
- `ToolSpec` / `ToolCall` / `ToolCallAccumulator`
- `SseDecoder`(插件协议不应自带 SSE 解析)
- `Plugin` / `PluginEffect` / `PluginInfo` / `PluginContext`
- `AuthProvider` / `AuthMethod` / `AuthStatus` / `SignInFuture` / `SignInNotifier`

**没有**(留在宿主,插件不该碰):

- `providers.toml` 的读写、配置路径、`codest_home()`
- 会话、rollout、TUI、`ModelPool` 的注册与缓存
- 插件安装器本身(`plugins.toml` 与生成的 crate 只由宿主读写)
- 任何 HTTP 客户端或终端代码

`ProviderInstance` 里的 `[providers.options]` 是插件私有配置:宿主只解析成 `serde_json::Map` 原样传下去,从不解释任何键。因此插件加一个开关不需要宿主改 schema;未知**顶层**键仍然是启动错误。

## 8. 测试

```bash
cargo test --workspace              # 宿主 + 契约 crate,全部离线
cargo clippy --workspace --all-targets -- -D warnings
python3 scripts/plugin_smoke.py     # 真实安装 → 带插件的二进制 → 卸载,13 项检查
```

`plugin_smoke.py` 会真的跑一次 `plugin install ../cst-provider-qoder`,用生成的二进制在伪终端里执行 `/status` 与 `/help`,断言插件 id 与命令出现,再 `uninstall` 并断言重建的二进制里已没有该插件;最后核对 `git status --porcelain` 前后一致。

`src/plugins/install.rs` 的单测覆盖:三种来源的清单渲染、`needs_patch` 的两种分支、路径来源的契约同源校验(不同副本直接拒绝)、符号覆盖、crate 名规范化与保留字拒绝、状态 TOML 往返、重复安装幂等、卸载后重新生成、缺 `Cargo.toml` 的目录、`--codest-src` 解析顺序。

## 9. 已知限制

- 本地插件与主程序**同生命周期**:改插件必须重新编译。这是编译期链接的代价,不是 bug——`codest plugin build` 只是一条包装命令。
- 没有沙箱与崩溃隔离:插件 panic 会带下整个进程。
- 生成物固定在 `~/.codest/plugins/crate/`,共享一个 `target/`:换 `--release` 或换宿主路径会重新编译依赖,但不会丢缓存。
- 一次只有一个 `codest plugin` 能改状态(`~/.codest/plugins.lock`);并发调用直接报错而不是排队。
- crates.io 来源要求 `cst-plugin-api` 已发布,否则 `[patch.crates-io]` 指向的路径依赖仍然生效——能编过,但语义上等于"本地契约 + 远端插件",版本漂移由 Cargo 报错兜底。git 来源的插件也必须按版本依赖契约(见 §2),相对 `path` 依赖的插件只能从本地目录安装。
- 写回 `plugins.toml` 与生成物都会**丢掉手写注释**(机器管理文件,不做保格式写回)。
