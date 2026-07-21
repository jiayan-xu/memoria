# Memoria 记忆智能性增强 — hy3 执行单（仲裁定稿）

**版本**: v1.0 执行定稿  
**日期**: 2026-07-20  
**来源**: Nova《memoria_improvement_plan.md》v1.0 + Cursor 仲裁 + 圆桌（default / analyst / kimik3 / qwen32）  
**圆桌日志**: `<workspace>/roundtable_memoria_plan_191332.log`  
**Memoria 裁决 id**: `5df1c124de2e2036`  
**实施对象**: hy3（编码）  
**性质**: 施工真源；与 Nova 原文冲突时 **以本单为准**

---

## 0. 一句话

采纳 B / A / C **能力**；拒绝 Memoria 热路径强制 LLM。  
顺序：**Phase0 → B → C → A**。Memoria 薄存储，脑子在 agent-core。

---

## 1. 硬约束（不可违反）

| ID | 约束 |
|----|------|
| H1 | Memoria = 身份/记忆**总线**（存取、检索、图、巩固游标）；**不**内嵌「每次 remember +1～2 次 LLM」主循环 |
| H2 | 提取 / 演化 / 护栏主逻辑在 **agent-core**（可复用 consolidate、text_signals LLM、Self-Evolution） |
| H3 | 遵守 HMS **O1–O6**：不作 `event_time` 写入主路径；ledger 默认仅 `memory_context`；冲突走 supersede，禁止 DELETE 当真值 |
| H4 | LongMemEval / LoCoMo = **回归指标**，非产品 KPI（勿为刷榜改架构） |
| H5 | 生产 DB 仅 `ADD COLUMN` / 可逆迁移；改前 GFS/`memory_backup`；密钥不入库 |
| H6 | 实现落点：canonical `memoria-open` + `agent-core`；运维树按双树纪律同步（`.NO_PUSH` 则只本地） |

---

## 2. 与 Nova 原文的差异（必须看）

| Nova 原文 | 本单 |
|-----------|------|
| `memory_remember` 同步触发提取 + 演化 | **禁止**热路径强制 LLM |
| 顺序 B → A → C | **Phase0 → B → C → A**（圆桌：C 先于 A，给演化可靠时序基线） |
| 方案 C 强调 4 时间戳新体系 | **贴**现有 `valid_from`/`valid_to` + supersede + `as_of`；仅补系统侧字段（若必需） |
| 智能进 memoria-core 写入链路 | B/A 主代码在 **agent-core**；Memoria 只加列、API、检索语义 |

---

## 3. Phase 0 — 技术债归零（约 1 周，阻塞后续）

> 未完成 Phase0，不得合入 B/C/A 生产路径。

### 清单

- [ ] **P0-1** 运维 Memoria 二进制与 `memoria-open` 目标 commit 对齐；确认 `CARGO_TARGET_DIR` 不写到沙箱导致「以为编了、托盘仍旧包」
- [ ] **P0-2** 验证 P3-0 语义/HNSW 通道：新写入记忆有向量或明确降级日志；embed `:8777`（或现网 embed）健康进 `/health`
- [ ] **P0-3** admin / `MEMORIA_JARVIS_BADGE` 与 K1+K3 文档一致；托盘与 `.env` 同源
- [ ] **P0-4** 清理 `agent_registry` 测试残留（≥20 行量级，以现网为准）
- [ ] **P0-5**（可选同迭代）密钥轮换若审计仍要求——单独清单，勿与功能 PR 混推

### 验收

- [ ] `/health`：memoria + embed pass  
- [ ] 新 `memory_remember` 后 hybrid/HNSW 行为可解释（有向量或明确无 embed）  
- [ ] registry 无测试垃圾身份  

---

## 4. Phase B — 写入前门提取压缩（agent-core 主，Memoria 辅）

**对标 Mem0；对应 Nova 方案 B。**

### 4.1 agent-core

- [ ] 在调用 `memory_remember` **之前**增加可选提取门（默认开可用 env 关，建议：`AGENT_MEMORY_EXTRACT=1` 默认开 / `=0` 关）
- [ ] LLM 输出：`facts[]` / `entities[]` / `preferences[]` / `relations[]` / `memory_type` / `actor`
- [ ] **1 raw → N 原子事实**：每条 fact 单独 `memory_remember`；用 `parent_id` / links 挂回父；`raw_ref` 可指向原文或旁路存储
- [ ] **失败降级**：LLM/超时/解析失败 → **原样** remember（行为与今日一致，不阻塞写入）
- [ ] **语义保真校验（圆桌强制）**：抽样或规则检查「压缩后关键实体/数字/日期不丢」；失败则降级原样 + 打审计/日志
- [ ] 中文 prompt；实体尽量走现有 `entity_*` / upsert

### 4.2 memoria-open（哑存储扩展）

- [ ] Schema（`ADD COLUMN`，可空）：`actor` / `memory_type` / `parent_id` / `raw_ref`
- [ ] 检索/profile：旧行 NULL 视为 `agent_inferred` / `declarative`
- [ ] **不**在 `remember_with_dedup` 内调 LLM

### 验收

- [ ] 长对话片段写入后，检索命中多为短原子事实，而非整段 raw  
- [ ] `AGENT_MEMORY_EXTRACT=0` 时与旧行为一致  
- [ ] 提取失败不丢写入  
- [ ] 既有测试 + 新增提取/降级/ns 隔离单测  

---

## 5. Phase C — 双时态补洞（贴现网，先于 A）

**对标 Zep 能力；对应 Nova 方案 C；实现贴 HMS/O2 既有模型。**

### 5.1 语义（必须写进代码注释/文档）

- `valid_from` = 世界上开始为真（t_valid）  
- `valid_to` = 世界上失效（t_invalid；NULL = 当前真）  
- supersede = 矛盾更新权威链（已有）；**失效不删**  
- 可选：`t_created` / `t_expired` 仅表示**系统记录/退役时间**（若与 `created_at`/supersede stamp 重复则可不加列，优先复用）

### 5.2 工作项

- [ ] 统一更新路径：事实变更 → 旧 tip `valid_to=now`（或 supersede 必写 valid_to，与 DESIGN Profile 一致）+ 新 tip  
- [ ] `memory_search` / hybrid：默认仅当前真值；`as_of=T` 返回历史窗口真值（与现有 `visible_as_of` 对齐，补测试）  
- [ ] 实体级：同实体多事实按 valid 窗口解释「当时为真」  
- [ ] **禁止**平行引入第二套四戳 API 命名混淆调用方  

### 验收

- [ ] 「2026-01 在 A 公司 / 2026-07 在 B 公司」：`as_of=2026-03` → A；`as_of=now` → B  
- [ ] 旧事实仍在库，非 DELETE  
- [ ] 与 isLatest / supersede 单测不回归  

---

## 6. Phase A — 记忆演化（Dream / consolidate 批处理）

**对标 A-MEM；对应 Nova 方案 A；热路径禁止同步演化。**

### 6.1 数据流

```
热路径 remember（可已是 B 的原子事实）
  └─ 可选：evolution_queue 入队（无 LLM）

Dream / consolidate（agent-core 批处理）
  ├─ 取队列或近邻 HNSW top-k（同 ns，k≤5）
  ├─ LLM：是否更新旧记忆 context/tags/边 / supersede
  ├─ 写 Memoria：evolved_* / links / evolution_log
  └─ 限流：复用 dream/consolidate 现有限流，防写风暴
```

### 6.2 memoria-open

- [ ] Schema：`evolved_context` / `evolved_at` / `link_count`（可空）  
- [ ] 表 `evolution_log`（new_id, target_id, change_type, old_value, new_value, model, created_at）  
- [ ] 回滚：按 `evolution_log.old_value` 可恢复  

### 6.3 演化滞后窗口（圆桌风险 — 必处理）

- [ ] 脏标记或「待演化」tip：recall/`memory_context` 可降权或标注  
- [ ] 文档写清：用户可见「未巩固」与「已演化」差异  
- [ ] 触发：夜间 Dream + 可选 `POST` 手动 consolidate（已有则复用）  

### 验收

- [ ] 写入「换了工作」后，经 consolidate，旧公司事实呈失效/已更新，且可 `evolution_log` 回滚  
- [ ] 热路径 QPS 不因演化 LLM 线性恶化  
- [ ] 限流单测 / 防风暴  

---

## 7. Phase 4 — 评测与打磨（非阻塞主路径）

- [ ] LongMemEval **完整集**作回归（报告数字，不绑发版门禁除非老大另定）  
- [ ] FTS5 中文（jieba 等）按需  
- [ ] HNSW 增量持久化 / 启动加速  
- [ ] `mcp_server` dispatch 单测补齐  

---

## 8. 风险登记（实施时对照）

| 风险 | 缓解（圆桌+仲裁） |
|------|-------------------|
| LLM 压缩丢关键实体/数字/日期 | B 语义保真校验 + 失败降级原样 |
| Dream 演化滞后，recall 到「错的旧事实」 | C 先落地时序；A 脏标记/降权；缩短关键 ns 巩固周期 |
| 时序补洞与 ledger/as_of 冲突 | C 只扩展现语义；加对齐测试 |
| 写入延迟 / 成本 | 热路径无强制 LLM；B 可关；A 仅批处理 |
| 263MB+ 生产库迁移 | 仅 ADD COLUMN；先备份 |

---

## 9. PR / 交付切分建议

| PR | 内容 | 仓 |
|----|------|-----|
| PR0 | Phase0 部署与债 | memoria 运维 + open（若有） |
| PR1 | Schema：actor/memory_type/parent_id/raw_ref | memoria-open |
| PR2 | 写入前门提取 + 保真 + 降级 | agent-core |
| PR3 | as_of/valid/supersede 补洞与测试 | memoria-open（+ 调用方若需） |
| PR4 | evolution_log + Dream 演化 | memoria-open + agent-core |
| PR5 | 评测/FTS/HNSW 打磨 | 按需 |

每 PR：测试绿、无密钥、无本机绝对路径；open push；运维按纪律同步。

---

## 10. 验收总表（全部 Phase 后）

- [ ] Phase0 验收全过  
- [ ] 跳槽场景：now / as_of 历史正确，旧行未删  
- [ ] 长文本 → 原子事实检索；关提取开关可回退  
- [ ] consolidate 后演化可见且可回滚  
- [ ] 现有 p0/p1/p2 HMS 与 profile 相关测试不破  
- [ ] 文档：更新 `OPTIMIZATION_*` 或本单链接；注明「Nova 原文已仲裁裁剪」  

---

## 11. 给 hy3 的开工指令

1. 只按本执行单，不按 Nova 原文热路径设计。  
2. 先开 **PR0 + PR1**，再 **PR2**；**PR3 必须先于 PR4**。  
3. 不确定处：优先「降级 + 可观测」，勿默默吞失败。  
4. 完成后在 Memoria `remember` 一条 decision（category=decision，tags=`hy3,memoria-intel`），并 @ 路径/commit。

**仲裁方**: Cursor（本会话）  
**方案作者**: Nova（WorkBuddy）  
**执行**: hy3  
