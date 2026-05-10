# TODO: agent-payment-sandbox

## Phase 1 — 项目骨架 ✅

- [x] 初始化 Rust workspace，单 binary 目标（`agent-payment-sandbox`）
- [x] 确认 CI 能在 macOS 和 Linux 上通过 `cargo build --release`
- [x] 定义 canonical 支付意图模型（`PaymentAttempt` struct）
  - 字段：`protocol`, `agent_id`, `task_id`, `merchant`, `amount`, `currency`, `network`（CAIP-2），`resource`, `mandate`, `payment_requirement`, `wallet`, `metadata`
- [x] 定义 `PolicyDecision` enum：`Allow`, `Block`, `RequireApproval`, `SimulateOnly`
- [x] 建立数据库抽象层（trait），同时支持 SQLite（demo）和 PostgreSQL（production）
  - 推荐用 `sqlx` + feature flags 区分两种后端
  - schema 版本化，不允许 SQLite/PG 跑不同 schema

---

## Phase 2 — Sandbox 核心 ✅

- [x] 实现 fake wallet 模拟器
  - 状态：balance available / reserved / settlement pending / succeeded / failed
  - 内存状态，进程退出时清除
- [x] 实现 `--state-file` 持久化（JSON），跨 run 保留余额和任务级累计消费
- [x] 实现 fake merchant 和 fake payment requirement 生成器
- [x] 实现 CLI 子命令 `agent-payment-sandbox sandbox`
  - 子命令：`status`, `seed`, `pay`
  - 支持 `--policy` / `AGENT_PAYMENT_POLICY` env var / `./agent-payment-policy.yaml` 加载顺序
  - 支持 `--state-file`
- [x] 实现 sandbox 模式下的 `require_approval` 同步 CLI 阻塞提示

注：policy 求值目前是 stub（永远 Allow），Phase 4 替换

---

## Phase 3 — Protocol Adapters ✅

- [x] 定义 `ProtocolAdapter` trait（parse → normalize）
- [x] 实现 x402 adapter
  - 解析 `402 Payment Required` body，归一化到 `PaymentAttempt`
  - 网络名映射到 CAIP-2，token 原子单位转 Decimal
  - 4 个单测覆盖正常 / 未知网络 / 无 extra / 空 accepts
- [x] 实现 AP2 mandate parser
  - 解析 VC-style mandate（credentialSubject + intent + constraints）
  - 元数据写入 `ap2_*` 字段（含 spec snapshot 日期）
  - `docs/ap2-adapter-changelog.md` 记录字段假设和已知 gap
  - 4 个单测覆盖正常 / context 覆盖 / 过期格式错 / 数组形 spending limit
- [x] 实现 x402 真实 sample integration（`examples/x402_cdp.rs`）
  - 用 alloy 做 EIP-712 签名，reqwest 发请求，base64 编 X-PAYMENT 头
  - feature flag `example-cdp` 隔离重依赖
  - 文档化 env vars (`X402_TARGET_URL`, `X402_PRIVATE_KEY`, 可选 `X402_VERBOSE` 等)

---

## Phase 4 — Policy Engine ✅

- [x] 实现 YAML policy 文件解析器（Phase 2 已经完成 schema + loader）
- [x] 实现全部 9 条规则的确定性求值：
  - max amount per payment
  - max total amount per task
  - max total amount per agent per day
  - merchant allowlist / blocklist
  - currency allowlist
  - network allowlist
  - payment frequency limit（per minute）
  - duplicate detection（merchant + resource + amount + time window）
  - require approval above threshold
- [x] policy 求值结果必须可重放（注入 `now` 参数；HashMap 不参与判定；deterministic test 验证）
- [x] policy 版本号写入每条 audit event（sandbox.rs 写 `policy_event.policy_version = rt.policy.version.clone()`）

---

## Phase 5 — HTTP Proxy ✅

- [x] CLI 子命令 `agent-payment-sandbox proxy` — `--listen`, `--policy`, `--state-file`, `--audit-db`, `--ca-dir`
- [x] 明文 HTTP 代理 + x402 拦截
  - 上游 402 challenge 解析、缓存（按 method+scheme+authority+path+query），写 `payment_attempt_created`
  - 客户端重放带 `X-PAYMENT` 时，base64 解码、查缓存（或回退合成）、跑 policy、放行/合成 402
  - **Allow → 转发上游，仅在 2xx 时写 `settlement_succeeded` + 计入 wallet 历史**；非 2xx/网络错 → `settlement_failed`，不计花销
  - X-PAYMENT 解析失败（base64/JSON/缺 authorization）→ 写 `payment_blocked` audit + 合成 402，不再静默 fail-closed
- [x] HTTPS MITM（CONNECT 升级 → 用本地 CA 签 leaf cert → tokio-rustls 终止 TLS → 解密后走相同的 x402 拦截路径）
  - 默认 CA 写到 `$XDG_CONFIG_HOME/agent-payment-sandbox/ca`（远离 repo root）
  - 每个 host 的 leaf cert 内存缓存；URI 重建保留端口
  - 启动时打印 `SSL_CERT_FILE=<path>` 提示
- [x] proxy 模式 fail-closed：上游错误/内部异常 → 502 + JSON 报错 + 写 `gateway_unavailable` 事件
- [x] proxy 模式 `require_approval` → 合成 402 + reason，不阻塞连接
- [x] 集成测试 `tests/proxy_integration.rs`：402 缓存 + replay、malformed X-PAYMENT、allow + upstream 500 不计花销、自定义端口 cache key 隔离

---

## Phase 6 — Audit Log（部分完成）

- [x] SQL schema（SQLite + PostgreSQL 共用 `src/storage/schema.sql`）
  - 表：`payment_events`，覆盖 timestamp/kind/payment_attempt JSON/policy_version/decision JSON/ap2_spec_version/raw_payload_ref/detail
- [x] 9 种 audit event 已经在 sandbox/proxy 路径中写入（PRD §7.5 的 8 种 + `gateway_unavailable`）
- [x] gateway 不可达 → 写 `gateway_unavailable` event
- [x] CLI 子命令 `agent-payment-sandbox audit`
  - [x] `audit list`：`--limit` + `--kind` 过滤
  - [x] `audit show <id>`：JSON 展开
  - [x] `audit export`：JSONL 一行一事件（newest first）
  - [ ] `audit replay <id>`：用历史 payload 重跑 policy 求值（待做）

---

## Phase 7 — Release

- [x] 配置 GitHub Actions
  - 在 `ubuntu-latest` 和 `macos-latest` 上 build + test
  - tag push 触发 release build
- [x] 打包 GitHub Release
  - `agent-payment-sandbox-linux-x86_64`
  - `agent-payment-sandbox-macos-aarch64`
  - `agent-payment-sandbox-macos-x86_64`
- [x] 写 README：10 分钟内跑通第一个 sandbox 支付流程
- [x] 提供 quickstart policy 示例文件（对应 PRD §11 样例）

---

## 待定 / 后续版本

- Homebrew tap
- `cargo install` 支持
- SDK（Python / TypeScript）
- 托管控制台 / SaaS 控制平面
- proxy 模式下 `require_approval` 异步审批队列
- 多币种 / 多网络 fake wallet
- Windows 支持
