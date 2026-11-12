# 外部插件

[English](../local-plugins.md) | 简体中文

外部插件是维护在本仓库之外、构建期链接进二进制的插件。`codest plugin install`
是唯一入口：一条命令记录插件、在本仓库**之外**生成 wrapper crate，并构建带该
插件的 codest 二进制——仓库本身零改动。插件契约本身见
[plugins.md](../plugins.md)。

## 1. 没有循环依赖的链接方式

源码里必须有某处引用 `&SOME_PLUGIN`，插件才会进二进制——插件是编译期链接的，
对外部插件也是如此。变化的是那"某处"住在哪里：宿主 checkout 之外的
`~/.codest/` 下由安装器生成的 wrapper crate。

契约类型（`Plugin`、`ProtocolFactory`、`ModelClient`、`ToolSpec`、
`AuthProvider`…）住在独立 crate
[`cst-plugin-api`](../../crates/cst-plugin-api/README.md) 里，这正是独立插件
仓库成为可能的原因：

```text
codest ──depends on──▶ your-plugin ──depends on──▶ cst-plugin-api
   └──────────────────── depends on ──────────────────┘
```

宿主依赖插件（为了链接），插件依赖契约 crate（为了实现 trait），codest 永远
不是插件的依赖——Cargo 层面没有循环。宿主把每个契约模块按历史路径重新导出
（`codest::core::model`、`codest::plugins::Plugin`…），所以这次拆分没有改名
任何东西。

**版本。** `PLUGIN_API_VERSION` 当前是 `1`。没有运行时握手：宿主与插件通过
**Cargo 版本**达成一致，契约不兼容时直接**编译失败**而不是启动失败——后者
才是糟糕的失败方式。新增带默认实现的方法不升版本；改已有签名或语义必须升。
依赖写范围（`cst-plugin-api = "0.1"`），不要精确钉死。

## 2. 安装

```bash
codest plugin install ../my-plugin                                    # 本地 checkout
codest plugin install my-plugin --git https://host/my-plugin.git --tag v1
codest plugin install my-plugin --version '^0.1'                      # crates.io
```

| 参数 | 含义 |
| --- | --- |
| target（位置参数） | checkout 路径、crates.io 包名，或 git URL |
| `--git <url>` | 从该仓库安装指定包 |
| `--package <name>` | 无法从 target 推断包名时手动指定 |
| `--rev` / `--branch` / `--tag` | 钉住 git 来源（互斥） |
| `--version <req>` | crates.io 安装的版本要求（默认 `*`） |
| `--symbol <name>` | 要链接的导出 `static`；可重复。默认 `PLUGIN`，或插件的 `[package.metadata.codest] plugins` |
| `--crate-name <name>` | Rust 标识符覆盖（默认包名 `-` → `_`；关键字被拒） |
| `--codest-src <path>` | wrapper 链接的 codest checkout（见下） |
| `--no-build` | 只重写列表与生成物，不跑 cargo |

target 直接给一个 git URL 也可以；URL 推不出包名时用 `--package` 补。重复
安装同一个包名会替换既有条目。

### 幕后发生什么

1. 插件条目写入 `~/.codest/plugins.toml`（原子替换；后续任何一步失败都会
   恢复先前状态）。
2. 在 `~/.codest/plugins/crate/` 重新生成 wrapper crate——在仓库之外，所以
   `git status` 永远干净，也永远不需要
   `git update-index --skip-worktree` 这类技巧。
3. 在那里 `cargo build`，打印产出的二进制路径。

wrapper 是一个普通 Cargo 包（`codest-local`），两个二进制、每个插件一行
依赖：

```rust
// ~/.codest/plugins/crate/src/main.rs（生成物——勿手改）
use codest::plugins::Plugin;

static LOCAL_PLUGINS: &[&dyn Plugin] = &[
    &my_plugin::PLUGIN,
];

fn main() {
    codest::cli::run_blocking("codest", LOCAL_PLUGINS);
}
```

`src/bin/cst.rs` 是同一份列表的 `cst` 别名。唯一的事实来源是
`~/.codest/plugins.toml`；生成物在每次 `install`/`uninstall`/`build` 时整体
重写，所以要改就改状态文件再重建，不要手改 wrapper。并发的 `codest plugin`
调用直接报错（锁文件，不排队）。

### `--codest-src`：wrapper 链接哪个宿主

wrapper 对一个 codest checkout 做 `path` 依赖。候选按顺序尝试，每个都校验
（`Cargo.toml` 声明包 `codest` 且 `crates/cst-plugin-api` 存在）；不合格的
候选降级到下一个：

1. 显式 `--codest-src`（显式路径不合格是硬错误，不降级）；
2. `$CODEST_SRC`；
3. 编译本二进制时的源码树（`CARGO_MANIFEST_DIR`，编译期固化）；
4. `~/.codest/plugins.toml` 里记录的 checkout。

所以 `cargo install` 装好的二进制仍能安装插件——它记着自己构建时的源码路径。
源码树被挪走时，显式传 `--codest-src` 即可。

### 契约同源

唯一的硬约束：宿主与每个插件必须把 `cst-plugin-api` 解析到**同一个** crate，
否则 `&MyPlugin` 不是 `&dyn Plugin`（编译期报错，不会到运行时才炸）。

- 插件按 `path` 依赖契约 → 安装器校验该路径就是宿主那份，不一致直接拒绝。
- 插件按 version/git 依赖契约（git 与 crates.io 来源必须如此）→ 生成的
  manifest 携带
  `[patch.crates-io] cst-plugin-api = { path = "<codest-src>/crates/cst-plugin-api" }`，
  让两边统一到宿主的副本。只在需要时才生成。

只会在与本仓库并列时构建的 checkout（对契约用相对 `path` 依赖）因此只能从
本地路径安装；克隆到别处会报 `no matching package named cst-plugin-api
found`，安装器会在错误后补上这段解释，且**不改动** `plugins.toml`。

## 3. 管理

| 命令 | 作用 |
| --- | --- |
| `codest plugin list` | 已装插件、来源、符号，以及链接的 codest 源码树 |
| `codest plugin uninstall <name>` | 删除一条并重建（其余插件保留）；`--no-build` 跳过构建 |
| `codest plugin build` | 按保存的列表重新生成 wrapper 并构建 |
| `codest plugin build --release` | 同上，开优化 |
| `codest plugin build --install` | 用 `cargo install --path … --force` 覆盖安装，而不只是构建 |
| `codest plugin build --codest-src <path>` | 换一个 checkout 重新链接 |

`--release` 与 `--install` 互斥：`cargo install` 总是构建优化版本，两个一起
传意味着其中一个被静默忽略——codest 直接拒绝这种组合。

构建不会隐式地从二进制里移除插件：不装就是不在这个二进制里，列表决定
wrapper 的内容。

## 4. 写一个可安装的插件

插件是一个普通的库 crate：依赖契约，导出若干实现 `Plugin` 的 `static`：

```toml
# Cargo.toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"

[dependencies]
cst-plugin-api = "0.1"          # 写范围版本，宿主副本才能与之统一
```

```rust
use cst_plugin_api::plugin::{Plugin, PluginContext, PluginError};

pub static PLUGIN: MyPlugin = MyPlugin;

pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn id(&self) -> &'static str {
        "my-plugin"
    }

    fn system_prompt(&self) -> Option<&'static str> {
        Some("Always answer in bullet points.")
    }
}
```

导出多个 `static` 时在 manifest 里声明一次，安装者就不必传 `--symbol`：

```toml
[package.metadata.codest]
plugins = ["PLUGIN", "TOOL_PLUGIN"]
```

插件能贡献的一切——命令、钩子、工具、协议工厂、登录入口——与内置插件实现
的是同一份契约；trait 参考见 [plugins.md](../plugins.md)，协议/登录路径见
[multi-provider.md](../multi-provider.md)。用 `cargo test` 独立测试插件
crate，它不需要宿主的任何东西。

## 5. 限制与设计笔记

- **改插件就要重建。** 插件改完需要 `codest plugin build`——这是编译期链接
  的代价，不是 bug。
- **没有沙箱、没有隔离。** 插件以宿主权限运行，panic 会带下整个进程。静态
  插件是可信代码；隔离应放在独立进程里，而不是这个系统里假装安全。
- **签名与校验尚未实现。** 没有任何机制认证插件来源；git/crates.io 来源的
  供应链风险与其指向的 registry 相同。
- **生成物由机器管理。** 重写会丢掉手写注释；`plugins.toml` 与 wrapper 都是
  整体覆盖。
- **同一时刻只有一个写者。** 并发 `codest plugin` 命令报锁错误，不排队。
- **crates.io 来源要求契约已发布。** `cst-plugin-api` 上架前，registry 安装
  的契约经由生成的 `[patch.crates-io]` 解析到本地 checkout——能编过，但生效
  的契约实际是本地副本。

## 6. 参考实现

`cst-provider-qoder` 是参考外部插件：Qoder 线协议 provider，贡献一个
`ProtocolFactory` 以及经由 auth 契约的浏览器与个人访问令牌登录。它维护在
独立仓库中，与 codest 一同发布。冒烟脚本
[../../scripts/plugin_smoke.py](../../scripts/plugin_smoke.py) 对它走一遍真实
安装路径：安装、在伪终端里用生成的二进制执行 `/status` 与 `/help`、断言插件
id 与命令出现、卸载、断言重建的二进制不再包含它，并核对全程 `git status`
保持不变。

## 7. 相关文档

- [plugins.md](../plugins.md) —— 插件契约与内置插件
- [multi-provider.md](../multi-provider.md) —— provider 实例、wire quirks、自定义协议
- [profiles.md](../profiles.md) —— profile 预设与 `tools` 过滤
- [../../README.md](../../README.md) —— 上手指南
- 设计笔记（中文）：[provider.md](../provider.md)、[session-id.md](../session-id.md)、
  [session-continue.md](../session-continue.md)
- 契约 crate 参考：[../../crates/cst-plugin-api/README.md](../../crates/cst-plugin-api/README.md)
