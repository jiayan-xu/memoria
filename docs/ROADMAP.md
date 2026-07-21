# Memoria 公开路线图 / Public Roadmap

> 脱敏公开版。本文件可安全提交到 GitHub `jiayan-xu/memoria`（`main`）。
> 不含任何密钥、内部审查文档或本机路径。

**定位**：AI Agent 的独立记忆中心。Rust 构建，MCP 原生，零外部依赖。
**状态**：W1（P0）/ W2（P1）/ W3（P2）全部完成。

## 已交付 / Shipped

### W1 — P0 安全与稳定底盘
- 授权矩阵全覆盖审计（authz 矩阵 + `tools_list()` 自检测试，未登记工具拒绝启动）
- 阻塞路径治理（HNSW / embedding 移出 async worker，spawn_blocking）
- 健康 / 密钥面收口（`memory_health`、单写者 PID 锁防双写）
- 单一部署形态（纯 Rust `memoria-server` 唯一生产入口）

### W2 — P1 记忆质量与巩固
- 检索评测集（eval/cases）+ 近义去重可靠化（P1-3）
- 偏好块 `memory_user_prefs`
- Dream 巩固流水线可靠化（P1-4：cursor 校验 / 限流 / session 语义）
- 轻量时序真值 `as_of`（P1-5：valid_from/valid_to + 时点查询）
- A2A 鉴权收紧（P1-6：消灭 `contains("admin")` 子串 → 精确 role + `to` 格式校验）

### W3 — P2 可运营与开源 DX（完成）
| 项 | 主题 | 状态 | 落点 |
|----|------|------|------|
| P2-1 | Tracing 可观测 | ✅ | `d5edd93`（与 agent-core 联调，x-trace-id 双边对接）|
| P2-2 | 配额与滥用防护 | ✅ | `ab257c9`（写入/搜索/备份三类限额，admin 豁免）|
| P2-3 | 实体图谱增强 | ✅ | `46aac10`（关系类型受控枚举 + mention 上下文搜索 + 证据下钻）|
| P2-4 | 导入导出与迁移 | ✅ | `3c1d8e9`（流式 export / 幂等 import / 迁移包 sha256 manifest）|
| P2-5 | Web Dashboard 产品化 | ✅ | `web/dashboard.js` + PUT/DELETE `/api/memories/{id}`（`X-Confirm: delete-memory`）+ 顶栏鉴权 |
| P2-6 | 开源 DX 与双树纪律 | ✅ | 示例 / docker-compose(loopback) / .env.example / README 基准 |
| P2-7 | PyO3 边界 | ✅ | `default = []`，`python` 可选；见 `docs/PYO3.md` |

## 设计取舍（公开 ADR 摘要）
- **不引入图数据库**：实体图谱用 SQLite 三表 + 受控关系枚举，保持零外部依赖。
- **时序真值走"轻量 as_of"**：valid_from/valid_to 列 + 时点查询，不维护整图时间版本。
- **巩固在 agent-core 侧**：Memoria 只提供哑 SQL 工具，LLM 提炼在 Python 侧，避免内置 Agent 主循环。
- **安全默认 loopback**：`MEMORIA_HOST` 默认 `127.0.0.1`；docker-compose 仅 `127.0.0.1:9003` 暴露。
- **强制 admin key**：未设或空的 `MEMORIA_ADMIN_KEY` 拒绝启动（禁止可预测自动 key）。

## 测试覆盖
- `cargo test` 全绿（截至 2026-07-13：单元 + 集成测试覆盖核心 / 配额 / 实体图谱 / 导入导出 / Dashboard API）。
- CI：GitHub Actions 在 ubuntu / windows / macos 三平台跑 `cargo check` + `build` + `test`。

## 双树纪律（贡献者须知）
- 唯一公开源：`jiayan-xu/memoria` 分支 `main`。
- 本地编辑从 canonical 副本（`memoria-open`）发起；另一工作副本 `memoria` 标记 `.NO_PUSH`，含内部审查文档，**勿推送**。
- 提交流程：先在 `memoria-open` 改 → 跑回归 → `git push origin main`；pre-push hook 校验 canonical 远端。
- 任何提交**不得含密钥或本机用户路径**。

## 9. 安全加固记录（2026-07-21，canonical 提交）

| 项 | 主题 | 落点 |
|----|------|------|
| P0-2 | 集成测试（`tests/eval.rs`）缺 `actor` 等列 panic；根因＝测试 bootstrap 仅调 `init_schema`+`init_core_tables` 漏跑增量迁移。已将 `migrate_*` 序列折叠进 `init_core_tables`，所有调用方拿到完整 schema。`cargo test --test eval` 转绿。 | `storage/sqlite.rs` |
| P1-③ | `evolution_log_query` 跨租户泄露：表无 `namespace` 列、查询无过滤、handler 不传 ns。已加 `namespace` 列（CREATE+幂等 ALTER）、INSERT 写 `namespace`、`evolution_log_query` 增 `namespace` 参数并按 `namespace = ?` 过滤、handler 透传校验后的 `ns`。 | `storage/sqlite.rs` / `tools/evolve.rs` / `mcp_server.rs` |
| P1-④ | ns 门控 `unwrap_or("default")` 静默落到默认命名空间。改为查 `PERMISSION_MATRIX` 的 `NsPolicy`：`None` 用调用者主 ns 并跳过校验；`NamespaceArg` 等要求显式 `namespace` 参数，缺失即拒；未登记工具维持历史行为。 | `mcp_server.rs` |
| P1-⑤ | release 构建仍向 stderr 回显默认 agent token 前缀。改为仅在 `debug_assertions` 下打印前缀，release 仅打 `agent_id`。 | `main.rs` |
| P1-⑦ | `/health` 回显具体 embedding 模型名/维度（信息暴露面）。改为仅回显可达性与 HTTP 状态。 | `health.rs` |

> 验收：`cargo test --test eval` 绿；`cargo build --release` 通过；`evolution_log_query` 仅返回调用者自身 `namespace` 行；`NamespaceArg` 类工具缺失 `namespace` 参数被拒。
