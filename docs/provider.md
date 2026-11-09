# `/provider`:添加 provider 与登录

> 状态:已实现(`src/tui/provider_wizard.rs` 状态机,`src/tui/app.rs` 渲染与执行,`src/core/config.rs::save_providers` 落盘)。
>
> 关键词:**一个入口、两条路径、插件只提供登录能力**。

## 1. 它解决什么

在此之前,加一个 provider 只能手改 `~/.codest/providers.toml`,而需要 OAuth/设备登录的厂商(例如 Qoder)连"手改"都不够——它还有一套签名协议和凭据刷新,必须靠插件实现。

`/provider` 把这两件事收进同一个菜单:

- 用 **base URL + API key** 新增一个 OpenAI 兼容端点;
- 为**插件贡献了登录入口的 provider** 完成登录(Qoder 的浏览器授权或 PAT)。

`/model` 只管"在已配置的模型之间切换";`/provider` 负责"把一个 provider 变成已配置"。

## 2. 菜单

```
/provider
  1 · Add an OpenAI-compatible provider   base URL + API key
  2 · Sign in to a plugin provider        credentials handled by the plugin
  3 · Show configured providers           read-only list
  4 · Close
```

`Sign in` 这一行只在**至少一个插件贡献了登录入口**时出现——没有插件的构建不会看到死路。`Show configured` 每行显示 `name · protocol · base_url · N model(s)`,插件服务其协议时再带一个状态徽标(`signed in` / `not signed in` / `sign-in expired`)。

按键:`↑↓` 选择,`Enter` 确认,`Esc` 返回上一步(在第一步则关闭),`Ctrl+C` 直接关闭向导(不会退出程序)。

## 3. 添加 OpenAI 兼容 provider

四个文本步骤 + 一个确认屏。文本输入复用编辑器,所以光标移动、粘贴、`Ctrl+U/K/W` 等都照常可用。

| 步骤 | 规则 |
|---|---|
| Route name | 必填,不能含 `/`(selection 在第一个 `/` 处切分),不能与已有实例重名 |
| Base URL | 必填,可以是 `https://host/v1` 或完整的 `/chat/completions` URL |
| API key | 可空——本地端点通常不需要;支持 `$VAR` 模板(写入文件后由宿主展开) |
| Model ids | 逗号或空白分隔,可空 = 让启动时发现 |

进入确认屏之前会**探测一次** `GET {base_url}/models`:

- 成功 → `all N discovered`;
- 失败 → `probe failed: …`,但**不影响写入**——端点可能没有 `/models`,手填的 id 照样能用。

确认屏:

```
write `gateway` to ~/.codest/providers.toml
endpoint http://127.0.0.1:8080/v1
key set · all 42 discovered
Enter write · Esc back
```

`Enter` 后的顺序是**先写文件,再改内存**:

1. `save_providers()` — 校验 + 临时文件 + `rename` 原子替换;
2. 失败 → 停在向导里报错,池与文件都不变;
3. 成功 → `ModelPool::install_instance()` 让新 provider 立即出现在 `/model`;
4. 后台跑一次模型发现,结果作为 notice 落到 transcript。

文件写失败时不改内存,是刻意的:**重启后加载到的内容必须与当前进程一致**。

### 写回会丢注释

`providers.toml` 由 `toml` crate 序列化,注释与排版会丢失(没有引入 `toml_edit`)。所以:

- 向导只在用户明确要求添加时才写;
- 手写维护的配置文件建议继续手改,或先在向导里加完再整理;
- 一旦用向导写过,该文件就变成"机器管理"的文件。

`body_overrides` 里的 `null` 会在写入时被剔除(TOML 无法表示 null),而不是让整次写入失败。

## 4. 登录插件贡献的 provider

### 4.1 菜单如何知道有登录入口

宿主向每个插件问三件事(`cst-plugin-api::Plugin`):

```rust
fn auth_providers(&self) -> &'static [AuthProvider] { &[] }
fn auth_status(&self, instance: &str) -> AuthStatus { AuthStatus::Unsupported }
fn begin_sign_in(&self, instance: &str, method: &str, secret: Option<String>,
                 notify: SignInNotifier, cancel: CancellationToken) -> Option<SignInFuture> { None }
```

`AuthProvider` 是一条静态声明:`id`(默认实例名)、`protocol`(它服务的协议)、`label`、`default_base_url`、`methods`。宿主把它和已配置的实例**按协议配对**:

- 某实例的 `protocol_id()` 命中某条 `AuthProvider.protocol` → 该实例获得登录行,`configured = true`;
- 某条 `AuthProvider` 没有任何实例在用 → 也列出,`configured = false`,登录成功后自动创建实例。

所以插件完全不需要知道 `providers.toml` 的存在,宿主也不需要知道任何厂商的 OAuth 细节。

### 4.2 状态是"本地、同步"的

`auth_status` 在构建菜单时**同步**调用,因此只能读本地文件,不能发网络请求。Qoder 插件读 `~/.codest/qoder/<instance>.json`:

| 状态 | 含义 |
|---|---|
| `SignedOut` | 没有存储的凭据 |
| `SignedIn { account }` | 有凭据且仍新鲜——**或在过期前能自行续期**(续期不需要人参与,所以不提示) |
| `Expired` | 有凭据、已过期、且无法自行续期,需要重新登录 |

"已登录"表示**存储中存在凭据**,不保证 token 此刻一定有效;真正失效时下一次请求会触发一次自愈重试。

### 4.3 一次登录的流程

1. 选中目标 → 方法选择(只有多于一个方法时才出现;首个是推荐项);
2. `SecretPrompt` 方法收集一个秘密值(`api_key` / token 的输入回显为 `••••`,`Enter` 后从输入历史中移除);
3. `Browser` 方法不带输入,直接启动;
4. 向导进入 `SignInRunning` 状态,后台任务通过 `SignInNotifier` 推进度:
   - 设备流的授权 URL 会作为 notice 显示(宿主**不会**替用户打开浏览器);
   - 轮询成功/失败都回到向导;
5. `Esc` 取消:置位 `CancellationToken`,插件应在每次 await 处响应;
6. 成功后:若原实例未配置,写入一条 `protocol` + `default_base_url` 的实例(不含 `api_key`——凭据归插件保管),然后 `install_instance` + 后台发现。

后台结果通过独立 channel(`ProviderTask`)回到事件循环,而不是直接改 UI:探测可能耗时数秒,设备流可能需要一两分钟。这与 `spawn_discovery` 的模式一致。

## 5. 运行时可变性

为了让向导生效,`ModelPool` 的注册表从"构造后不可变"变成 `RwLock<Arc<ProviderRegistry>>`:

- `registry()` 返回 owned `Arc` 快照——调用方在遍历期间不需要一直持有锁;
- `install_instance()` 用与启动相同的校验重建注册表,并**逐出该 provider 的 client 缓存**(旧 client 里烘焙着旧的 `base_url`/`api_key`,留下就会把请求发到上一个端点);
- `remove_instance()` 同样清理模型发现缓存;
- `ModelPool::client()` 对未注册 provider 从 `expect` 改为返回 `SelectionError::UnknownProvider`,向导引入的路径因此不会 panic。

`App.pool` 与 worker 的 `Arc<ModelPool>` 是同一个分配,所以安装后 `Op::SwitchModel` 立刻能解析。

## 6. 测试

- **纯状态机**(`src/tui/provider_wizard.rs`):动作行随插件变化、四步文本推进、重名/空值/斜杠拒绝、探测失败仍可确认、单方法直连 / 多方法选择、notice 与完成后的状态复位、`Esc` 逐级返回。
- **App 层**(`src/tui/app.rs`):`/provider` 打开并压掉命令条、实例与插件协议配对(已配置/未配置两种)、文本步骤不进模型、重名提示、**落盘 + 安装**并保留其他 provider。
- **Config 层**:`save_providers` 往返、目录创建、二次写入覆盖、`null` 剔除。
- **Pool 层**:`install_instance` 注册新 provider、替换生效、client 缓存逐出、未知协议被拒且状态不变、`remove_instance` 幂等。

## 7. 不做

- **`$EDITOR` 编辑流**:向导已覆盖增量修改,批量编辑用编辑器改文件更快。
- **删除 provider**:`ModelPool::remove_instance()` 已就绪,但菜单还没接;在那之前删除仍是手改文件。
- **`toml_edit` 保格式写回**:多一个依赖,换来保留注释。
- **向导里配置 `headers` / `body_overrides` / `[providers.wire]`**:这些是长尾调整,手写更清楚。
