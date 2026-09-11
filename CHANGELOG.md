# 演进日志 / CHANGELOG

## 2026-09-11 — v0.4.0

### 回填 0.3.0 之后未记账的功能（2026-08-17 ~ 08-29）
- **HyPE 假设问句嵌入 V1**（PR #6）：查询侧 HyDE 双索引，OCR 多轮加固后合入。
- **LongMemEval 完整集回归 harness**（PR #10）：500 问全评分入库 `eval/longmemeval/`。
- **SessionWatcher 落点可配置**（PR #11）：`MEMORIA_WATCH_NS`。
- **重排层近精确语义匹配 boost**（PR #12）：修复完整@5 召回回归。
- **向量重建守护**（PR #13）：`rebuild_vectors.py` batch 16→8 + 零向量拒绝重试。
- **HNSW 层级 RNG 种子固定**（PR #14/#15）：根治跨重启召回抖动。
- **本地嵌入服务** `embed_local_server.py`（:8778）：默认模型 Qwen3-0.6B → **bge-m3 int8**（0.6B 召回不达标，MRR 0.55→0.325）。
- **`MEMORIA_TRACE=1` 分段计时**：auth / query-embed / spawn_blocking-queue / dispatch-run / hybrid_search。
- **P1 写入整合（mem0 式）+ P2 Kuzu 时序图**（PR #16）：`consolidation.rs` 判 ADD/UPDATE/NOOP（fail-open）；`graph_kuzu_server.py` :8779 + MCP `memory_graph_query`。
- **OCR 安全加固**：cypher 只读护栏 v2、NOOP 无目标防丢数据、as_of 时效补全。

### 本轮：运维可观测 + 发版锚点补齐
- **`memory_backup_verify`**（MCP，admin）：校验备份归档 manifest/sha256/integrity；restore 仍走 CLI `memoria-server backup restore`（fresh-target-only）。
- **`memory_ops_status`**（MCP，admin）：图谱空名实体 / 整合 ADD·UPDATE·NOOP·FAIL_OPEN 分布 / 向量覆盖 / 备份清单 / `recall_alert.json` 快照一屏可读。
- **`consolidation_log` 表**：每次写入整合判定落账（含 FAIL_OPEN / UPDATE_BAD_TARGET），支撑 NOOP 率与误 UPDATE 治理。
- **健康软检查 `graph_consolidation`**：空名实体、向量覆盖率、24h 整合 FAIL_OPEN 率进 `/health`。
- **`recall_guard.py`**：写 `recall_alert.json`（severity=warn/elevated + 连续劣化次数），可被 `memory_ops_status` / 夜间巡检读取。
- **chore(release)**：bump 0.3.0 → **0.4.0**。合并后打 annotated tag `v0.4.0`。

---

## 2026-08-17

### SessionWatcher 观察落点可配置（MEMORIA_WATCH_NS）
- **改动**：`src/session_watcher.rs` 观察写入命名空间由硬编码 `"default"` 改为环境变量 `MEMORIA_WATCH_NS`（缺省 `"default"`，保持历史行为）；新增 `watch_namespace()`，启动日志打印目标 ns（`[SessionWatcher] Observations ns: ...`）。
- **动机**：观察与 consolidate/dsh 写入散落不同 ns，夜间巩固无原料可提炼。本机部署以 `MEMORIA_WATCH_NS=agent/xujiayan` 统一落点。
- **不 bump 版本**：当前 `0.3.0` 保持。

---

## 2026-08-10

### 版本管理补齐（发版纪律落地）
- **改动**：新增 `docs/RELEASING.md` 发版纪律；追补历史 tag `v0.2.0`（`7245341`，2026-07-05 初始发布）与 `v0.3.0`（`f4428bf`，2026-07-21 bump），使版本可回溯。
- **动机**：`Cargo.toml` 版本号在涨（0.2→0.3）但**从未打 tag、从未写 CHANGELOG**，123 commits 无可复现稳定点。
- **说明**：本条目合入时**不 bump 版本**——当前 `0.3.0` 保持，下个功能版再 bump。

---

## 2026-07-21 — v0.3.0

- **chore(release)**：bump memoria 0.2.0 -> 0.3.0（`f4428bf`）。
- **P2-5 dashboard authz**：dashboard 授权补齐。
- **P2-7 PyO3 默认关闭**：PyO3 绑定改为默认关闭，编译面收窄。
- **部门文档入库 + 图谱详情 `?id=` 规避 path 404**。

---

## 2026-07-05 — v0.2.0（初始发布）

- **初始发布**：Memoria v0.2.0 — Rust-native MCP memory server（`7245341`）。
- 记忆入库 / 检索 / 图谱 / A2A 桥接等基础能力成型。
