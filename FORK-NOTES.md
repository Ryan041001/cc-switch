# Ryan041001 的 CC Switch 二开版

初始基于官方 **v4.0.12**。维护分支为 `main`，保留 `feat/pi-usage-observability` 作为最初开发分支。后续正式版由自动同步维护，不把二开功能还原成官方原版。

- 上游：<https://github.com/farion1231/cc-switch>
- Fork：<https://github.com/Ryan041001/cc-switch>
- 原来的 `cc-switch-antigravity` 是工作副本，`antigravity-cc-switch-bridge/cc-switch-antigravity-sync.patch` 是同一份改动的补丁包；**不应重复套用**。此分支已经包含实现，不再依赖外部补丁。

## 为什么没有给 Pi 强行套用 Claude/Codex 的代理接管

需求是拿到首字、TPS、用量，而不是改供应商路由。Pi 的供应商配置是多模型、可叠加的，OAuth 与自定义 provider（例如 Antigravity ACP）也不一定经过可改写的 HTTP `baseUrl`。为了统计而伪造统一 API、替换 OAuth 或覆盖 `models.json` 会破坏原来的调用链。

这里实现的是 **Pi 实时指标观测模式，而不是 HTTP 转发代理**。不提供 Pi 本地路由、故障转移或代理模型切换，也不将它们标成“已接管”。

## 已合并的功能

### 1. Antigravity 原生用量与原生计时

- 只读扫描 `~/.gemini/antigravity-acp/conversations/*.db`；先校验当前官方 ACP 程序内的 protobuf schema。
- 保留已经落库的 request ID、semantic ID 和 `pi_session` 去重账本命名空间，避免升级重复计费。
- token、缓存、思考拆分取自原生数据；成本按 CC Switch 本地模型定价表计算，**不是 Google 账单**。
- 将原生计时标记为 `antigravity_native`，不再当成历史日志估算。
- 原生 `time_to_first_token` 和 `streaming_duration` 是不同阶段：总耗时按两者相加，生成窗口为后者。旧补丁把后者直接写作总耗时，可能出现“耗时比首字短”；此分支会在有原生记录可核对时修复自己标记的历史明细，不改 token、已计价成本或账本。
- 每个 UTC 日期第一次写入前进行 SQLite 在线备份，沿用 `~/.config/antigravity-cc-switch-bridge/backups/`。

**重要：ACP 可能缓冲输出再一次性吐给客户端。** 本机原生记录中存在等待数秒、随后数毫秒吐出的情况，这不是数十万 token/s 的模型生成速度。沿用 CC Switch 的保护门槛：输出至少 100 token、首字后的生成窗口至少 100ms 才显示 TPS；不满足时留空，不伪造数据。原生计时描述的是原生模型调用，不是 Pi 外层整个 Agent 轮次。

### 2. 普通 Pi 调用的实时首字、TPS、耗时

打开 **用量 → 数据来源 → Pi 实时指标（不接管网络）**，启用“安装本地计时观测扩展”。之后在每个已运行的 Pi 中执行 `/reload`，或重启 Pi。禁用后同样需要 `/reload`。

扩展安装到当前选定的 Pi agent 目录：

```text
<agent-dir>/extensions/cc-switch-metrics.ts
```

目录解析沿用 Pi 现有规则（CC Switch 的 Pi 目录覆盖设置、`PI_CODING_AGENT_DIR`、默认 `~/.pi/agent`）。它不会修改 `auth.json`、`models.json` 或 `settings.json`，不会占用监听端口。修改过、同名冲突或符号链接路径不会被覆盖/删除。

计时口径：

- 使用 `performance.now()`，不从历史日志猜时长。
- 从 `before_provider_request` 到第一个非空文本/思考/工具参数增量，记录首字；到 `message_end` 记录总耗时。不把工具执行时间算进生成时间。
- HTTP 重试会重置到新尝试；不沿用失败尝试的首字。
- 自定义 provider 未触发 HTTP 钩子时，从 `turn_start` 实测，来源单独标成 `pi_observed_turn`，不冒充网络请求 TTFT。
- 在 `turn_end` 后写一条不进入模型上下文的 `cc-switch.metrics.v1` custom entry，只含持久化消息 ID、消息时间戳、计时及口径；不复制 prompt、正文、原始事件、token、费用或密钥。
- 会话扫描用相同 request ID 更新已有 Pi 用量行，来源为 `pi_observed` / `pi_observed_turn`。即使指标晚到、文件重写或重复扫描，也不新增计费记录。
- TPS = 输出 token /（总耗时 − 首字），沿用上述低输出/短窗口门槛。不流式上报有效增量时首字保持为空，不写成 0。

### 覆盖边界

- 仅覆盖启用后、有正常持久化消息 ID 的主 Agent 轮次。不开会话持久化、禁用扩展、压缩调用、工具内部的额外模型调用不补时序。
- 历史请求无法补测客户端首字；原有会话估算仍保留 `≈` 标识。
- provider 必须自己上报真实 usage 才能从 Pi 行计算 TPS。
- **Antigravity ACP 当前向 Pi 上报 0 token**：不将外层轮次与原生库中的多个模型调用按时间强行配对。它的 token 和原生首字/耗时仍由上面的原生导入器提供；扩展不会凭空新增一个 0 token 请求或重复计费。
- 数据刷新走原有会话扫描/“立即同步”。安装在磁盘上的状态不代表一个未 `/reload` 的 Pi 进程已经加载它。

## 与官方自动同步

已配置 [Sync upstream](https://github.com/Ryan041001/cc-switch/actions/workflows/sync-upstream.yml)：

- 每 6 小时检查一次官方最新**正式 Release**（不含草稿、预发布）。GitHub 定时任务可能排队/延迟，不是秒级同步。
- 默认不追未发布的官方 `main`；手动运行时可选 `main`。
- 没有新版本就成功退出，不改分支、不报错。
- 在每次运行独有的临时分支中执行普通 Git merge，保留 fork 的历史及改动。不使用 `ours/theirs` 消掉冲突，不使用强推或重置维护分支。
- 完整调用现有 CI（前端、Linux/macOS/Windows 后端、WSL2 测试），检查的是**合并后的确切 SHA**。全部通过后才快进更新维护分支 `main`。
- 合并冲突或检查失败：保留原来的 `main`，Actions 标红，并开一个指派给仓库所有者的 Issue。后续失败更新同一 Issue，不每 6 小时刷一个新 Issue；通过验证并完成同步后自动关闭。是否收到邮件由 GitHub 通知设置决定。
- 验证期间若你自己提交了新代码，旧候选不会覆盖它：本次延后，下次重新合并验证。
- 临时分支在结束后清理；有变化/失败时保留 14 天诊断 artifact（冲突列表、候选补丁等）。失败运行可以在 Actions 页面手动重跑。
- 默认只使用 GitHub 自动提供的仓库 Token；完整 CI 的 checkout 不保留写凭据，也不继承维护 Token。

手动检查：Actions → **Sync upstream** → **Run workflow**，选择分支 `main`、channel `release`。勾选 `verify_current` 可在没有新版本时也强制跑完整 CI。

### 权限与边界

仓库 Issues 需要保持启用，否则只能在 Actions 日志中看到报错。新建 fork 的 Actions 可能需要手动启用；GitHub 也可能因公共仓库长时间无活动而停用 schedule，届时需要在 Actions 重新启用。

GitHub 的内置 Token 在部分涉及 `.github/workflows/` 的写入上可能被拒绝。**不会偷偷复用或上传你本机的登录凭据**。如果日志明确提示 workflow 权限不足，需要你创建仅授权本 fork 的细粒度 Token，授予 Contents 与 Workflows 读写权限，存为仓库 Actions secret **`UPSTREAM_SYNC_TOKEN`**。同步的两个写入阶段会优先使用它；CI 测试阶段不接收该 secret。权限/网络故障也会安全停止并报错，不能承诺除了文本冲突之外永远不会失败。

GitHub 上的 `main` 会自动推进，但本机工作目录不会被远程任务偷偷改写。本地跟进时，在干净工作区切到跟踪 `fork/main` 的 `main`，执行 `git pull --ff-only`；有未提交改动时先自行提交/备份，不要强制重置。

自动同步的是**仓库源码**，不是你电脑里已安装的 App。本 fork 尚无自己的 updater 签名密钥，不会自动下载官方包覆盖二开版。构建和 App 升级仍按下一节执行；源码同步不修改你的本地配置或统计库。

## 构建与更新

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm typecheck
corepack pnpm build:renderer

# macOS 当前架构，仅生成 app，不覆盖 /Applications 下现有安装
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  corepack pnpm exec tauri build --bundles app \
  --config src-tauri/tauri.fork.conf.json \
  --config '{"build":{"beforeBuildCommand":""}}'
```

构建产物：`src-tauri/target/release/bundle/macos/CC Switch.app`。通用二进制可加 `--target universal-apple-darwin`，需先安装两个 Rust target。

**必须带 fork 配置构建**：它移除了官方更新下载地址，避免自动安装官方包覆盖本分支功能。不生成 updater 签名制品；此 fork 尚未配置自己的签名密钥和 `latest.json`，暂时应手动升级。继承的官方公钥不能用来发布 fork 的自动更新。源码的上游默认配置仅用于保持合并方便，不应直接拿来发布二开包。

旧版安装与补丁留在原来的目录作为历史备份；本次源码开发不自动替换已安装 App、不改写真实统计库，也不自动安装 Pi 扩展。通过界面明确启用后才会安装扩展。

## 验证

```bash
corepack pnpm typecheck
corepack pnpm exec vitest run
corepack pnpm build:renderer
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib session_usage_pi
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib pi_observation
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib session_usage_antigravity
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage_stats
```

Antigravity 的 `--ignored` 测试只读本机原生数据库，导入目标是临时内存库与临时备份目录，绝不写真实 CC Switch 统计库。
