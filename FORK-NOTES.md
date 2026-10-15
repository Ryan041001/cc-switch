# CC Switch 分支定制与维护规范说明

本仓库基于官方 **v4.0.12** 版本创建。主维护分支为 `main`，历史特性分支 `feat/pi-usage-observability` 归档保留。本分支通过自动化流程跟进官方稳定发布版本，并在合入过程中确保既有定制能力完整保留。

- 上游仓库：<https://github.com/farion1231/cc-switch>
- Fork 仓库：<https://github.com/Ryan041001/cc-switch>
- 架构说明：原 `cc-switch-antigravity` 为独立开发工作副本，`antigravity-cc-switch-bridge/cc-switch-antigravity-sync.patch` 仅为早期离线补丁集合。当前 `main` 分支已完全内化该实现，后续开发与构建均基于源码分支进行，不再依赖外部补丁。

---

## 架构决策：Pi 指标观测与代理机制解耦

### 背景与设计原则

Pi 运行时的核心观测目标为精准采集首字延迟（TTFT）、生成吞吐率（TPS）及 Token 消耗用量，而非干预或重写模型调用链路。

Pi 具备高度可扩展的多模型叠加配置体系，其 OAuth 认证流与自定义 Provider（如 Antigravity ACP）并不依赖通用的反向代理 `baseUrl`。若采取类似 Claude / Codex 的 HTTP 反向代理接管方案（如伪造统一 OpenAI 接口、拦截 OAuth 或覆盖 `models.json`），将破坏 Pi 原生调用链路的稳定性与扩展性。

### 观测模式定位

因此，本项目采用 **非侵入式实时指标观测模式**：

- **不提供** 本地网络路由接管、故障转移或代理模型重定向；
- **不篡改** 既有模型供应商配置；
- UI 层面明确区分“实时性能观测”与“网络代理接管”，避免语义混淆。

---

## 定制功能特性

### 1. Antigravity 原生用量解析与高精度计时

- **数据库检索**：以只读方式检索 `~/.gemini/antigravity-acp/conversations/*.db`，并在解析前对当前官方 ACP 运行时的 Protobuf Schema 结构完整性进行断言校验。
- **幂等性与去重保证**：严格复用已持久化的 Request ID、Semantic ID 及 `pi_session` 去重账本命名空间，避免版本升级或重复扫描引发用量重复统计。
- **精确计量归因**：Prompt / Completion Token、上下文缓存（Cache Read/Creation）及思考耗时（Thinking）均提取自原生协议数据。计费金额严格基于 CC Switch 本地模型定价表换算，仅供成本核算参考，非云厂商实际账单。
- **计时语义标准化**：
  - 原生采样记录标记为 `antigravity_native`，区别于基于历史会话推断的近似统计（`≈`）。
  - 分离 `time_to_first_token`（首字响应延迟）与 `streaming_duration`（内容生成流耗时）。总耗时由两者严格累加计算，生成时间窗口仅计流式传输阶段，消除“总耗时小于首字时间”的时序异常。
- **存储安全性**：在每个 UTC 日期发生首次数据写入前，调用 SQLite 在线热备份接口自动将主数据库备份至 `~/.config/antigravity-cc-switch-bridge/backups/`。
- **低吞吐与批量输出保护**：
  - ACP 客户端可能存在响应缓冲机制，导致数据在等待后集中回传，该现象属于传输缓冲而非模型生成吞吐达到数十万 Token/s。
  - 速度遵循全局统一的流式规则：仅持续流式响应计入（输出 ≥ 100 Token、生成窗口 ≥ 100ms）。ACP 客户端存在缓冲机制，`streaming_duration` 可能只是几百毫秒的排空时间而非生成窗口，因此原生导入器仅在流式窗口 ≥ 1 秒时才将记录标记为流式（`is_streaming`）；突发回传按非流式入库，TPS 留空并在详情说明，原生 TTFT 照常保留。统计层对所有供应商共用同一规则，无按渠道单独调参。
  - 原生计时精确反映底层模型推理周期，不与 Pi 顶层 Agent 的复杂轮次生命周期混同。

### 2. Pi 会话实时性能指标监控（TTFT / TPS / 耗时）

- **启用与加载路径**：
  - 在 **Pi → 供应商页** 或 **用量 → Pi** 中启用实时性能观测；原有 **设置 → 数据来源** 开关仍可使用。Pi 的供应商列表继续来自原生 `models.json`，无需在 CC Switch 重复配置或切换到路由模式。
  - 启用或禁用扩展后，需在已运行的 Pi 实例中执行 `/reload` 命令或重启 Pi 进程以生效。
  - 扩展脚本安装位置遵循 Pi 标准插件目录规范：
    ```text
    <agent-dir>/extensions/cc-switch-metrics.ts
    ```
  - 目录解析严格遵从优先级规则（CC Switch 自定义目录覆盖 > `PI_CODING_AGENT_DIR` > 默认 `~/.pi/agent`）。该扩展不触碰 `auth.json`、`models.json` 或 `settings.json`，不占用任何本地网络端口；对于已被用户修改、存在命名冲突或符号链接的文件实施保护，拒绝静默覆盖或删除。
- **高精度度量管道**：
  - 统一采用 `performance.now()` 单调时钟进行测量，杜绝依赖不可靠的历史日志时间戳推断。
  - 所有 Pi 主 Agent 供应商共享同一套观测：优先从 `before_provider_request` 计时；没有该 HTTP 钩子的自定义供应商从 Pi 的公共 `context` 模型调用钩子计时；旧运行时再退回 `turn_start`。数据源分别记为 `pi_observed`、`pi_observed_call` 与 `pi_observed_turn`，详情标明不同起点，绝不把客户端调用前置时间冒充网络 TTFT。
  - 捕获首个非空文本/思考/工具参数增量作为首个输出时间，至各自 `message_end` 结束。一个用户轮次内的每条 assistant 模型响应（包括工具调用前后的多次请求）单独计时，并在 `turn_end` 后按持久化消息对象的精确身份关联，工具执行时间不会混入任一生成窗口。
  - 网络重试触发时自动重置计时基准，不继承失败尝试的测量偏移。
  - Pi 的公共 `context` 钩子覆盖标准配置供应商和扩展 Provider；它是模型调用前的客户端边界，之后的请求准备/认证仍包含在时长中，单独标记为 `pi_observed_call`。仅旧 Pi 运行时没有可用模型调用钩子时，才从 `turn_start` 采集端到端指标并标记为 `pi_observed_turn`。
  - 每轮结束后为轮内每条已持久化 assistant 模型响应分别写入 `cc-switch.metrics.v1` 诊断条目，仅包含持久化消息 ID、单调计时结果及度量口径；严禁包含 Prompt 文本、消息正文、Token 明细、密钥或敏感配置。
  - 会话扫描器基于统一 Request ID 对既有用量记录执行幂等更新，无论采集延迟或重复触发，均保证用量记账唯一性。
  - 实时吞吐率计算遵循 `TPS = 生成 Token / (总耗时 - 首字延迟)`，若未取得流式有效增量则保持为空值，严禁填充零值或伪造数据。

### 3. 功能覆盖边界

- 仅度量显式启用该扩展且具备可精确关联持久化消息的 Pi 主 Agent 模型响应；无持久化会话、禁用扩展模式、上下文压缩调用及嵌套工具内部自行发起的模型调用均不在监控范围内。一个用户轮次里多次由主 Agent 发起的工具调用前/后模型请求会分别记录。
- 历史调用记录无法追溯客户端首字延迟，历史会话依然保留估算标识（`≈`）。
- 需由模型服务商或上游驱动上报有效 Usage 载荷，方可计算 TPS 指标。
- **关于 Antigravity ACP 当前报告 0 Token 的说明**：系统严禁将外层会话轮次与底层数据库内的多个原生模型调用强行按时间戳映射。其 Token 用量与高精度计时指标仍由原生导入器提供，扩展层不会产生冗余的 0 Token 计量记录。
- 前端“立即同步”基于会话扫描逻辑触发。扩展文件在磁盘上的就绪状态不代表尚未执行 `/reload` 的运行中 Pi 进程已完成加载。

---

## 上游自动化同步规范

项目已配置 GitHub Actions 自动化工作流 [Sync upstream](https://github.com/Ryan041001/cc-switch/actions/workflows/sync-upstream.yml)，规范如下：

- **同步调度机制**：
  - 每 6 小时自动巡检官方最新的正式 Release（过滤 Draft 与 Pre-release）。由于 GitHub Actions 调度队列机制，实际触发时间存在正常容差，非实时触发。
  - 默认仅跟进官方正式发布版本；工作流手动触发时支持指定跟踪官方 `main` 分支。
  - 若检测到上游无版本演进，工作流正常退出，不变更分支状态，不产生非预期告警。
- **分支合并与验证策略**：
  - 每次同步在独立的临时分支中执行标准 Git Merge，完整保留本分支历史记录与定制特性。严禁使用 `ours/theirs` 策略消除合并冲突，严禁使用强制推送（`--force`）或硬重置（`hard reset`）。
  - 合并生成候选版本后，调用完整 CI 矩阵（包含前端构建、Linux / macOS / Windows 后端构建及 WSL2 兼容性验证），且仅在**合并后确切 Commit SHA** 上进行严格校验。
  - 仅当全部持续集成流水线成功通过后，维护分支 `main` 方可通过快进（Fast-Forward）更新发布。
- **异常处置与故障隔离**：
  - 若发生代码合并冲突或 CI 校验失败，维护分支 `main` 保持现有稳定状态不予变更，Actions 运行标记为失败，并自动创建指派给仓库所有者的追踪 Issue。
  - 若持续处于未解决状态，后续失败运行将统一更新该 Issue 状态与最新诊断信息，避免产生冗余通知。在冲突修复并通过验证完成同步后，系统将自动关闭该 Issue。
  - 验证运行期间若维护分支接收到新的人工提交，系统自动终止旧候选分支的合并推广，确保任何情况下不发生提交覆盖；待下一执行周期重新基线化处理。
  - 临时工作分支在执行结束时自动清理；存在变更或失败时将诊断 Artifact（包含冲突上下文、候选补丁）持久化归档 14 天，支持后续排查与手动重跑。
  - 运行环境默认仅授予最小权限的 GitHub Token，流水线 Checkout 步骤禁用持久化写入凭据。

### 手动触发验证

在 GitHub 仓库界面访问：**Actions** → **Sync upstream** → **Run workflow**。

- 分支选择：`main`
- channel 参数：`release`（正式版）或 `main`（跟踪主干）
- `verify_current` 参数：启用时可在无版本更新的情况下对当前分支强制触发全量 CI 验证。

### 权限与运行边界说明

- 仓库需保持开启 **Issues** 功能，否则故障通知将仅留存于 Actions 日志流中。
- 在新建 Fork 仓库中，Actions 可能处于待激活状态，请确保已手动启用；若公开仓库长期无活跃提交，GitHub 可能会暂停定时调度，需按提示重新恢复。
- GitHub 默认 `GITHUB_TOKEN` 在涉及 `.github/workflows/` 目录写操作时可能受权限策略限制。工作流**绝不读取或上传本地开发环境的认证凭据**。若工作流因权限不足报错，可在仓库设置中创建专属细粒度 Token（需配置 `Contents` 与 `Workflows` 读写权限），并配置为 Actions Secret `UPSTREAM_SYNC_TOKEN`。该 Secret 仅在合并准备与发布阶段由受信任脚本使用，CI 测试矩阵阶段对其物理隔离。
- 本地工作目录与远端仓库保持完全解耦。远端 `main` 分支推进后，本地仅需在干净工作区内切换至跟踪分支执行：
  ```bash
  git pull --ff-only
  ```
  若本地存在未提交改动，应先行暂存或提交，严禁执行破坏性重置。
- 自动同步机制的范围限定为**仓库源码版本**，不触及本地已安装的桌面应用程序实体。本定制版本尚未配置独立的发布签名密钥链与差分更新服务，不会自动下载官方安装包覆盖本地客户端。

---

## 本地构建与交付规范

### 构建命令

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm typecheck
corepack pnpm build:renderer

# macOS 平台构建命令（仅生成本地 App 包，不覆盖 /Applications 下现有安装）
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  corepack pnpm exec tauri build --bundles app \
  --config src-tauri/tauri.fork.conf.json \
  --config '{"build":{"beforeBuildCommand":""}}'
```

构建交付产物位于：`src-tauri/target/release/bundle/macos/CC Switch.app`。如需构建跨架构通用二进制，可追加 `--target universal-apple-darwin` 参数（需预先配置对应 Rust Target 交叉编译环境）。

### 构建配置安全约束

**必须采用 Fork 专用配置构建**：

- `src-tauri/tauri.fork.conf.json` 已显式剔除官方自动更新服务端点，防止客户端静默下载官方发行版进而覆盖定制特性。
- 当前分支尚未建立专有签名密钥体系与 `latest.json` 自动更新服务，暂不支持应用内热更新，版本升级均通过本地构建或手动安装交付。
- 上游默认配置仅在合并对比时作参考对齐，严禁直接用于编译发布二进制安装包。

---

## 质量验证套件

```bash
corepack pnpm typecheck
corepack pnpm exec vitest run
corepack pnpm build:renderer

# 后端单元与集成测试
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib session_usage_pi
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib pi_observation
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib session_usage_antigravity
PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=stable \
  cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage_stats
```

注：带有 `--ignored` 标记的 Antigravity 测试用例以只读方式对接本地数据库，执行目标被严格限定于临时内存数据库与临时隔离备份路径，严禁对运行中的 CC Switch 真实统计库产生写操作。
