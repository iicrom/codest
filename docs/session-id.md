# codest 会话 id 与 session 头

> 状态：已实现（`src/core/session_id.rs`、`src/core/rollout.rs`、
> `src/core/openai_compat.rs`、`src/core/providers.rs`）。UUIDv7 会话 id 贯通
> 文件名、协议与上游 session 头；`wire.session_header` / `models.session_header`
> 可控制发送与否。
>
> 关键词：**一个 id、eager-id/lazy-file、逐轮传值、配置化头名、默认发送**。

## 1. 问题

改造前 codest 有三套互不相连的"会话"概念：

| 层 | 标识 | 位置 |
| --- | --- | --- |
| 磁盘 | `format!("{:x}", now_nanos())`，同时是文件名与 `SessionMeta.id` | `rollout.rs` |
| 进程内协议 | 创建顺序 `usize` | `Op::SwitchSession{session}`、`sessions[current]` |
| TUI | `views[i]` / `active_view` 位置镜像 | `tui/app.rs` |

持久 id 已经存在但是**惰性的**：写入后只被 `list_workspace_sessions()` 从文件名
解析回来排序，从不进入协议、UI 或插件（`PluginInfo` 无 id 字段）。

更硬的约束是 **id 与文件被焊死在同一函数**：`RolloutWriter::start_session()`
既生成 id 又 `create_new` 建文件，且只在首个 `UserMessage` 时调用。于是第一轮
请求时**根本没有 id 可用**——想给上游发 session 头，这个耦合必须先拆。

## 2. 参考实现

| | pi-coding-agent | opencode Go | codex |
| --- | --- | --- | --- |
| id 类型 | `uuidv7()` 字符串 | 客户端自选 | `ThreadId`（UUIDv7 newtype） |
| id 何时生成 | 建 session 时 | — | `thread/start` 时立即（`reserve_thread_id`） |
| 文件何时创建 | 建 session 时 | — | 延迟到首个 turn（`deferred_creation`） |
| resume 入口 | `--continue` / `--resume` / `--session <id>` | — | `resume <uuid\|name>` / `--last` / picker |
| resume 是否重造 id | 否 | — | 否，且用一调用就 panic 的生成器写测试钉死 |
| 发给 provider | 不发 | 要求 `x-opencode-session` | `session-id` + `thread-id` + `x-client-request-id` 头，body `client_metadata`，`prompt_cache_key` |

codex 的做法最有借鉴价值，因为它是三者中唯一同时解决两件事的：

1. **eager-id / lazy-file**：`thread/start` 就分配 id，rollout 文件用
   `deferred_creation` 推到首个 turn，注释直言
   `// Fresh threads have no rollout until their first turn.`。id 早于文件存在。
2. **上游暴露是"配置 + 逐轮传值"**，不是硬编码。

## 3. 决策

**3.1 一个 id：UUIDv7。**

同一个值回答"这是哪个会话"：rollout 文件名、`SessionMeta.id`、worker 与 TUI 的
寻址键、上游 session 头取值。选 UUIDv7 而非 codex 式自增或旧 hex-nanos：

- 时间有序 → 文件名字典序即创建顺序，`/sessions` 与 `--continue` 依赖这一点；
- 标准可粘贴格式，为将来的 `cst --session <uuid>` 留好形状。

**3.2 eager-id / lazy-file。**

`RolloutSlot` 在会话创建时就持有 id（`new_session_id()`），文件仍然懒创建。
恢复的会话从 `LoadedSession.meta.id` 取 id，**绝不重造**——这是会话身份跨重启
不变的前提，有测试钉住。

**3.3 头名归构造，头值归回合。**

- 头名（`session-id` / `x-session-id` / 不发）是**配置**，在 `build_client` 时
  解析后烘进 client。
- 头值是**会话状态**，随 `/sessions` 切换而变，走 `TurnOptions.session_id`
  逐轮传入。

这条分工解释了为什么 `ProtocolFactory::create(instance, model)` 的签名不需要
改动：它已经同时拿到 instance 和 model，足够解析头名。外部 provider 插件
（`cst-provider-qoder` 等）零改动，`PLUGIN_API_VERSION` 保持 1。

**3.4 client 每轮重建，底层连接池进程级共享。**

`ModelPool` 原先按 `selection` 缓存 client。带会话头之后这是个陷阱：两个会话用
同一个 `provider/model` 会命中同一个缓存项，头值就固化成第一个会话的 id。因此：

- 删除 `ModelPool::clients` 缓存；`pool.client()` 每次构建；
- `ModelBackend::Pooled` 不再持有 client 字段；
- 但 `reqwest::Client`（连接池）提到进程级 `OnceLock`，所以"每轮重建"只重建
  轻量配置结构体，不会每轮重新 TCP/TLS 握手。

重建频率是**每 turn 一次**（`TurnContext` 在 turn 开始时构建，turn 内各 round
复用），不是每 round。

**3.5 默认发送，可按 provider 或 model 关闭。**

需求是"默认需要"，所以 `WireQuirks::default().session_header == "session-id"`。
三档语义：

| 配置 | 效果 |
| --- | --- |
| 不写 | 发 `session-id: <uuid>` |
| `wire.session_header = "none"` | 该实例不发 |
| `[[providers.models]] session_header = "none"` | 只关掉这个模型 |

`"none"` 与既有的 `EFFORT_FIELD_NONE` 同构，词汇一致；空字符串则视为笔误
（`validate()` 报错），不会被当作关闭。

**3.6 不发 `x-opencode-session`。**

opencode 文档要的是它自己的拼写，但它同时声明识别 codex / Claude Code / ZCode
的**原生** session 头。发 `session-id`（codex 拼写）覆盖面更广，且配置项允许
需要别的拼写的网关自行改名，不必为每个上游硬编码一种。

## 4. 落点

| 文件 | 改动 |
| --- | --- |
| `src/core/session_id.rs` | 新增：`new_session_id()`（UUIDv7）、`is_session_id()` |
| `src/core/rollout.rs` | 文件名 `rollout-<uuid>.jsonl`；`start_session(id, …)` 接受外部 id；`list_workspace_sessions` 只认 UUID 名并按名字排序 |
| `src/core/session.rs` | `RolloutSlot` 持 id；`RunWorker` 三处创建点分配/继承 id；两处 `TurnOptions` 填 `session_id`；`ModelBackend::Pooled` 去掉 client 字段 |
| `src/core/providers.rs` | 删除 client 缓存；`build_client` 解析头名（model 覆盖 instance） |
| `src/core/openai_compat.rs` | `with_session_header()`；`session_headers()`；进程级共享 `reqwest::Client`；会话头排在静态头之后 |
| `crates/cst-plugin-api/src/config.rs` | `WireQuirks::session_header`、`ModelInfo::session_header`、`DEFAULT_SESSION_HEADER`、`SESSION_HEADER_NONE`、`resolve_session_header()` |
| `crates/cst-plugin-api/src/model.rs` | `TurnOptions::session_id` |

会话头在 `stream_chat` 里排在静态 `headers` 循环**之后**，因此逐轮的会话 id
覆盖 `providers.toml` 里手写的同名头——与 `/effort` 覆盖 `body_overrides` 的
优先级一致。

## 5. 兼容性

- **旧 rollout 文件不兼容**：按决策，hex-nanos 命名的文件不再被识别为会话，
  `list_workspace_sessions` 直接跳过。理由是新旧格式字典序混排会错乱，与其
  猜不如明确不认。
- **配置向后兼容**：`WireQuirks` 有表级 `#[serde(default)]`，`ModelInfo` 的
  新字段带 `#[serde(default)]`，所以改造前写的 `providers.toml` 照常加载，
  并获得"发 `session-id`"的默认行为。
- **插件契约不变**：`TurnOptions` 只是新增字段（插件作为消费者只读不构造），
  `ProtocolFactory::create` 签名未动。

## 6. 验证

- `cargo test`：371 条离线全绿。会话相关新增覆盖
  `a_fresh_session_has_an_id_before_it_has_a_file`、
  `resuming_a_session_reuses_its_persisted_id`、
  `switching_sessions_switches_the_id_sent_upstream`、
  `session_header_resolves_instance_then_model`、
  `the_session_header_carries_the_turn_session_id`、
  `listing_skips_files_that_are_not_session_ids`。
- `python3 scripts/session_header_smoke.py`：驱动真实 `cst` 打本地测试服务器
  （`--dump-headers` 落盘请求头），断言默认发 `session-id: <uuid v7>`、
  `wire.session_header = "none"` 与 model 级 `"none"` 都不发。
- `python3 scripts/pty_smoke.py`：与本改动前的基线对比，失败集合完全一致
  （同一批 pty 时序敏感用例，非回归）。

## 7. 未做

- **`cst --session <uuid>` 与 `/session`**：id 已经贯通并在磁盘上可寻址，但
  CLI 入口与展示尚未接上。当前寻址仍是创建顺序索引，协议层保留 `usize`。
- **插件可见性**：`PluginInfo` 还没有 id 字段。`context_store::session_dir()`
  早已按 rollout id 参数化，是第一个现成的消费者。
- **body 侧标识**：`prompt_cache_key` 之类的 body 字段当前可用
  `body_overrides` 静态配置，但无法引用运行时会话 id。
