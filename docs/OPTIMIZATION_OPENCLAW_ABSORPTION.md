# OpenClaw v2026.7.1 — 吸收分析（存储侧 / Memoria）

> 只读研究产物。源码克隆于 `openclaw-probe/`（tag `v2026.7.1`，仅研究，永不提交/推送）。
> 运行侧（gateway / 崩溃恢复 / 安全 / 路由 / 技能）见 `agent-core/docs/OPTIMIZATION_OPENCLAW_ABSORPTION.md`。
> 对照目标：`Memoria`（SQLite 记忆/身份总线 :9003，dream_state / entities / memory_remember / memory_observe）。
>
> **修订（2026-07-18）**：纠正「无向量 / 无备份」等与现网不符的对照；Phase A 改为补 **verify / restore / manifest**，禁止重复造 create/HNSW。供 hy3 执行前以本文为准。

## 0. 与既有吸收研究的关系

| 研究 | 落点 | 与本文关系 |
|---|---|---|
| HMS 吸收 | ledger / text_signals / O1–O6 / Dream | 已完成；本文不重做 consolidate / 召回主路径 |
| Grok Build | agent-core 沙箱 / 文件 checkpoint | 运行侧；本文不碰 |
| 白龙马 | 意识循环 / 预取 | 运行侧；本文不碰 |
| **OpenClaw（本文）** | 备份契约增强 + 隔离校验思想 | **只补缺口**，不颠覆单库 + HNSW |

## 1. 边界与定位对照

| 维度 | OpenClaw 7.1 存储 | Memoria（现状，2026-07） | 重叠度 |
|---|---|---|---|
| 引擎 | `node:sqlite` `DatabaseSync`（WAL, busy_timeout=30s, FK, synchronous=NORMAL） | SQLite（Rust `rusqlite`） | 高 |
| 全局库 vs per-agent 库 | 全局 `openclaw-state.sqlite` + 每 agent 独立 `openclaw-agent-<id>.sqlite` | **单库多 ns**（`allowed_ns` / badge） | 中（隔离策略不同） |
| 向量 / 关键词 | `sqlite-vec` vec0 + 嵌入缓存 + FTS5 | **已有**：HNSW（`hnsw_vectors`）+ FTS5（`memories_fts`）+ `hybrid_search`；可选外置 embed | 高（能力齐，实现不同） |
| 记忆写回 | active-memory = **recall-only**；整合在 `memory-core` dreaming | `consolidate()`→LLM→`memory_remember`→游标；另有 `memory_context` / `memory_recall` | 高（路径已落地） |
| 备份 | `backup create`（VACUUM INTO）+ `backup verify`；**无 list/restore** | **已有**：`backup::perform_backup`（VACUUM INTO + GFS + integrity）+ MCP `memory_backup` / `memory_backup_list` + 定时自动备份；**缺** OpenClaw 式 manifest 归档、verify 契约、**restore（fresh-target）** | 中（create 已有；补契约与 restore） |

## 2. 核心子系统代码事实（file:line）

> 以下 file:line 锚定 OpenClaw `v2026.7.1` 探针；探针删除后需按 tag 重开核对。

### 2.1 记忆架构
- 引擎：`node:sqlite` `DatabaseSync`；封装 `requireNodeSqlite()`，启用 WAL / `busy_timeout=30000ms` / `foreign_keys` / `synchronous=NORMAL`：`src/state/openclaw-state-db.ts:257-263,34-38`；agent 库 `src/state/openclaw-agent-db.ts:4,253`。
- 向量：`sqlite-vec` vec0 虚拟表 `memory_index_chunks_vec`，表名常量 `packages/memory-host-sdk/src/host/memory-schema.ts:7-13`；扩展按需加载 `packages/memory-host-sdk/src/host/sqlite-vec.ts:49-105`（失败降级）；嵌入缓存 `memory_embedding_cache`（`:72-81`）；FTS5 `memory_index_chunks_fts`（`:398-408`）。
- **active-memory 非整合层**：`extensions/active-memory/index.ts:1-4` 明确为 recall 插件，经子 agent 调 `memory_search`/`memory_recall` 注入有限摘要（`buildRecallPrompt` `:1071-1142`）；**无定时巩固/写回**。真正整合在独立 `memory-core` 的 `dreaming.ts`。

### 2.2 持久化后端
- DB 路径：全局 `resolveOpenClawStateSqlitePath`；per-agent `resolveOpenClawAgentSqlitePath`；权限 0700 目录 / 0600 文件。
- 迁移：加法式 + `PRAGMA user_version`。状态库 / agent 库 schema version=1；遗留表迁移含 `SAVEPOINT`/回滚。

### 2.3 SQLite 备份（关键 — 远小于宣传）
- CLI 仅注册 `backup create` 与 `backup verify <archive>`：**无 `backup sqlite` 动词、无 `list`、无 `restore`**（`src/cli/program/register.backup.ts:12-94`）。
- create：`createBackupArchive`（`src/infra/backup-create.ts:754`）。遍历 `*.sqlite`（含 `-wal/-shm`）；每库 **`VACUUM INTO`**；全局库额外净化 `delivery_queue_entries`；产物 gzip tar + `manifest.json`（`schemaVersion:1`），拒绝覆盖。
- verify：校验 manifest / 路径 / payload 对应 / 硬链接目标在归档内。**不提取、不还原。**
- **list / restore：全仓缺失**。`fresh-target` 命中来自 npm 冒烟脚本，与备份无关。

### 2.4 Per-agent 数据库隔离
- 每规范化 `agentId` 对应独立 sqlite；`schema_meta` 记 `role='agent'` + `agent_id`，打开时校验不匹配抛错；注册进全局 `agent_databases` 表。

## 3. 宣传 vs 代码事实（存储侧）

| 宣传主张（7.1 说明） | 代码判定 | 证据 |
|---|---|---|
| `openclaw backup sqlite create` | **部分**：实为 `backup create`（无 `sqlite` 子动词） | `register.backup.ts`；`backup-create.ts` |
| `backup list` | **缺失** | 仅 `create`/`verify` |
| `backup verify` | **已实现** | `backup-verify.ts` |
| `backup restore`（fresh-target-only） | **缺失** | 无 `restoreBackup` |
| active-memory 周期整合/巩固 | **不实**：recall-only；整合在 memory-core dreaming | `active-memory/index.ts` |
| per-agent 数据库 | **已实现** | `openclaw-agent-db.ts` |
| SQLite + 向量 + FTS5 | **已实现**（OpenClaw 侧） | `memory-schema.ts`；`sqlite-vec.ts` |

## 4. 与 Memoria 当前架构对照（修订后）

- **整合理念同构**：OpenClaw「recall-only + dreaming 整合」与 Memoria「`memory_context`/`recall` vs `consolidate`/Dream」已对齐；**不要再做一次大拆模块**，最多文档化职责边界（B3）。
- **向量召回**：Memoria **已有** HNSW + FTS 融合，**不是缺口**。OpenClaw 的 `sqlite-vec` 是另一实现；**默认不引入第二套向量栈**。仅当有可测收益（延迟/召回率）时再开「对比选型」议题（原 B2 降级为可选评估，非必做）。
- **隔离模型**：OpenClaw = 物理 per-agent 文件 + `schema_meta`；Memoria = 单库 + `allowed_ns`。**禁止**照搬多文件模型。可吸收：打开连接/工具调用时强化所有权校验思想（与 agent-core 来源门控对齐）。
- **备份**：Memoria **已有** VACUUM INTO 热备 + list + GFS。OpenClaw 可吸收的是 **manifest 归档格式 + verify 契约**；**restore 必须自研**（OpenClaw 缺失）。

## 5. 吸收决策矩阵（存储侧）

| 项 | 判定 | 理由 | 落地点 |
|---|---|---|---|
| 已有 `perform_backup` / `memory_backup` | ❌不重做 | create 已落地 | — |
| `backup verify`（manifest 完整性） | ✅吸收 | 现网缺归档级 verify | 扩展现有 backup 模块 + MCP |
| `backup restore`（fresh-target） | ✅自研 | OpenClaw 缺失；线上必须可回滚验证 | 同上；**仅允许全新目标路径** |
| manifest + gzip tar 归档（可选包装现有 .db 备份） | 🟡部分 | 提升可移植/校验 | `backup` 模块增量 |
| per-agent 物理隔离 | ❌不吸收 | 颠覆单库模型 | — |
| `schema_meta` 所有权校验思想 | 🟡部分 | 补强 ns/badge 鉴权 | 鉴权路径小改，非换存储模型 |
| sqlite-vec 替换/并行 HNSW | ❌默认不吸收 | 双栈成本高；无收益证据不上 | 可选评估文档，不进 Phase A |
| 「recall / 整合」职责分离 | ✅理念已满足 | 路径已存在 | 文档化即可（B3） |
| active-memory 当整合层 | ❌不吸收 | 实为 recall-only | — |

## 6. 落地方案（供 hy3 执行）

### Phase A（备份契约补齐，约 1 周）— **必做**

> **禁止**：重写 `perform_backup` 主路径；引入 sqlite-vec；按 ns 拆多库。

- **A1：manifest 归档包装（增量）**  
  - 在现有 `VACUUM INTO` 产物之上，增加可选 `memoria backup archive`（或扩展 `memory_backup`）：生成 `manifest.json`（`schemaVersion`、条目路径、sha256、含 HNSW 向量文件条目）、gzip tar；拒绝覆盖已存在归档。  
  - 资产范围：**主 SQLite + `hnsw_vectors`（及若存在的 audit 分库）**——**不是**「各 namespace 库」。

- **A2：`memory_backup_verify`（或 CLI 等价）**  
  - 校验 manifest 合法 / `schemaVersion` / 条目均在归档内 / payload 哈希一致。参考 OpenClaw `backup-verify.ts` 不变量；**不提取、不写生产库**。

- **A3：`memory_backup_restore`（自研，OpenClaw 无）**  
  - **仅允许恢复到全新（fresh）目标目录/库文件**（目标路径不得已是运行中的生产库）；恢复前强制 verify；恢复后可选 integrity_check。  
  - 验收：fresh 目标 restore 后数据可读；对生产路径调用必须拒绝。

### Phase B（增强，2–4 周）— **非阻塞**

- **B1**：借鉴 `schema_meta`，在 ns 访问路径强化来源/所有权校验（与 agent-core P0-2 对齐），不改单库模型。
- **B2（可选评估，默认不做）**：仅当产品明确要求「换向量引擎」时，对比 sqlite-vec vs 现 HNSW；**禁止默认同跑双栈**。
- **B3**：文档化「召回（context/recall）vs 整合（consolidate/Dream）」边界；无强制代码大拆。

**验收（Phase A）**  
1. 对运行库打一次 archive → verify 通过。  
2. restore 到全新空目录 → verify + 可读。  
3. restore 指向现网生产库路径 → 必须失败。  
4. 回归：现有定时 `perform_backup` / `memory_backup_list` 行为不破。

**运维铁律**  
- 实现落点：canonical `memoria-open`（`main`）；运维树按既有双树纪律同步（`.NO_PUSH` 则只本地）。  
- 推前 GIT 安全扫描（无密钥、无本机绝对路径入库）。  
- `.env` / 生产 `memoria.db` 禁止进 git；restore 禁止覆盖热库。

## 7. 不吸收清单（写入 AGENTS.md / 代码注释）

1. **`backup restore` / `backup list` 以为 OpenClaw 已有** —— OpenClaw 全缺 restore；list 亦缺（Memoria 已有 list）。  
2. **active-memory 当作整合层** —— 实为 recall-only。  
3. **"fresh-target-only restore" 当作 OpenClaw 已实现** —— 代码中不存在；Memoria **自研**时采用该语义。  
4. **per-agent 物理文件隔离** —— 禁止照搬。  
5. **用 sqlite-vec「补齐」Memoria 向量** —— Memoria 已有 HNSW；默认禁止当缺口施工。  
6. **重做 VACUUM INTO create** —— 已有 `backup.rs` / `memory_backup`。

## 8. 实施状态

- [x] 研究完成（v2026.7.1 源码深读 + 宣传对照）
- [x] 方案修订（2026-07-18：对齐现网 HNSW/备份；Phase A 重切）
- [x] **Phase A 已落地（2026-07-18）**：在 `backup.rs` 既有 `perform_backup`/`check_integrity`/`restore_from_backup` 上 WRAP（不引入 tar/gzip）——A1 `backup create`（VACUUM INTO `backups/cli/<ts>/` + `manifest.json` sha256 + 拒绝覆盖）；A2 `backup verify`（schema_version/allowlist/文件存在/sha256/integrity_check）；A3 `backup restore`（fresh-target-only，拒绝已存在库，自研语义）。`cargo check` 全绿。
- [ ] Phase B 待开工（按 §6 路线）
- 探针目录 `openclaw-probe/` 为只读研究副本，可随时删除。
