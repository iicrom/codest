# 快速开始

[English](../getting-started.md) | 简体中文

安装 codest、指向一个端点、跑起第一次会话。本页是这次起步所需的全部参考：环境要求、两种安装方式、快速开始的各种走法，以及完整的 CLI 参数面。

## 环境要求

- Rust **1.88+**（edition 2024）——仅构建需要；二进制之外没有运行时依赖。
- 任意终端。除 `bash` 工具用到的 Unix 进程组处理外没有平台相关代码。

## 安装

```bash
# 从 git 安装
cargo install --git https://github.com/iicrom/codest

# 或从本地 checkout 安装
git clone https://github.com/iicrom/codest
cd codest
cargo install --path .
```

安装后会得到两个等价命令：

```bash
codest   # 主命令
cst      # 简称别名，CLI 完全相同
```

## 快速开始

### 连接真实端点

```bash
export CST_API_KEY=sk-...
export CST_BASE_URL=https://api.openai.com/v1
export CST_MODEL=gpt-4o-mini            # 可选；不给则由 /model 从端点发现模型列表
codest
```

或显式传参：

```bash
codest --api-key sk-... --base-url https://api.openai.com/v1 --model gpt-4o-mini
```

说明：

- `--base-url` / `--api-key` 按**成对**要求工作（单端点路径）。只给其一时 codest 照常启动，在第一次发消息或打开 `/model` 时提示补全另一个。
- 没有配置 `providers.toml` 时，flag/env 会合成一个一次性端点实例；一旦存在 `~/.codest/providers.toml`，同样的 flag 退化为**覆盖所选实例的字段**（见 [multi-provider.md](../multi-provider.md)）。
- 模型发现在启动时后台执行；发现的元数据（上下文窗口、effort 等级）供 footer 与 `/model` 使用。

### 完全离线

```bash
# 终端 1 —— 本地 OpenAI 兼容测试服务（仅依赖 Python 标准库，不校验 key）
python3 scripts/openai_test_server.py --port 8080

# 终端 2
codest --api-key test --base-url http://127.0.0.1:8080/v1 --model test-model
```

测试服务支持 SSE 流式输出、`GET /models`，不校验 key，并会把消息里的关键词（`@bash`、`@read`、`@write`、`@edit`、`@tool`）转成真实工具调用——足以在没有外网的情况下验证整条 agent 链路。

也可以完全不起服务：在 provider 实例上声明 `protocol = "test"`，使用内置的进程内 echo 后端。

### 恢复会话

```bash
codest --continue    # 直接激活本工作区最近一次保存的会话（-c 等价）
codest --resume      # 同上，但先打开会话选择菜单
```

## CLI 参考

| 参数 | 环境变量 | 说明 |
| --- | --- | --- |
| `--model <id>` | `CST_MODEL` | 启动时的模型 id（选择的 `model` 半边）。 |
| `--provider <name>` | `CST_PROVIDER` | 启动时的 provider 实例。 |
| `--base-url <url>` | `CST_BASE_URL` | OpenAI 兼容 base URL（或完整 `/chat/completions` URL），覆盖所选实例。 |
| `--api-key <key>` | `CST_API_KEY` | 端点 API key，`--help` 中隐藏。 |
| `--system <text>` | — | 覆盖系统提示词。 |
| `--continue`、`-c` | — | 继续本工作区最近一次保存的会话。 |
| `--resume` | — | 加载全部已保存会话并在启动时打开选择菜单。 |
| `--profile <name>` | `CST_PROFILE` | 以 `~/.codest/profiles.toml` 中的某个 profile 启动。 |
| `--tui-mode <mode>` | `CST_TUI_MODE` | `fullscreen`（默认）或 `inline`；`regular` 是 `inline` 的别名。 |

其他环境变量兜底：`CST_*` 未设置时，`OPENAI_API_KEY` / `OPENAI_BASE_URL` 供合成的单端点实例使用。

### `codest plugin` 子命令

```bash
codest plugin install <路径 | --git <url> | crates.io 包名>   # [--rev|--branch|--tag] [--symbol NAME]...
codest plugin uninstall <name>
codest plugin list
codest plugin build [--release | --install]
```

install 的具体行为（在仓库外生成 wrapper crate 并构建）见 [local-plugins.md](../local-plugins.md)。

## 下一步

- [configuration.md](configuration.md) —— `config.toml`、`providers.toml`、`profiles.toml`、session 头与全部环境变量。
- [commands.md](../commands.md) —— 全部 slash 指令及其行为。
- [sessions.md](../sessions.md) —— rollout 持久化、`--continue` / `--resume`、中断恢复。
- [tools.md](../tools.md) —— `bash` 与 `fs` 工具、会话级开关、工具轮数。
- [tui.md](../tui.md) —— 按键、fullscreen 与 inline、Markdown 渲染与 thinking 卡片。
- [architecture.md](../architecture.md) —— `Op`/`Event` 总线、会话 worker、模块地图。

[← 返回 README](../../README.md)
