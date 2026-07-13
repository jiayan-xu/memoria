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
