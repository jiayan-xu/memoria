# 演进日志 / CHANGELOG

## 2026-10-29 — 修复记录时间为 UTC（用户实测 WB 接力时暴露）

### 缺陷
memories 业务时间线全链用 `chrono::Utc::now()`（写入 created_at/updated_at、observe、
evolve、profile as_of、recall 的 last_recalled、entities 时间戳），中国单时区用户在
WB 里看到「2026-10-29 02:50 写入」（实际本地 10:50），时间直觉全错。

### 修复（memories/entities 业务链 8 处切 `chrono::Local`）
- remember(2)/observe/evolve/profile now_iso/hybrid as_of 基线/mcp_server entities
  与 last_recalled——存储、过滤、排序同一口径，整体自洽。
- 连带修 hybrid recency 时区错位隐患：`now_secs` 与 `last_recalled` 解析（and_utc）
  保持同构口径（本地挂钟按同一折算），差值恒等于真实差值；存储切 Local 后新记忆
  不会被误判 8 小时旧而遭降权。
- 保持 UTC 不动：health/auth token/imp_exp 导出/审计 rfc3339（带 Z 的运维面，
  不参与 memories 时间线）。
- 新旧记录混排安全：旧记录 UTC 值恒小于切换后 Local 值（+8h），created_at 排序
  单调不回退。

## 2026-10-29 — 修复交接协议 v1 读侧断链（memory_context 不出 handoff 锚点）

### 缺陷
协议 v1（#31）把锚点以 `category='handoff'` 落库，但 `memory_profile` 的 dynamic
查询白名单只有 `decision/fact/pattern`——**按协议写的锚点永远进不了新会话上下文**。
此前未被发现的巧合掩盖：上一份锚点恰好以 `decision` 类目写入。10-29 凌晨端到端
复测（memory_observe 写 → memory_context 读）暴露：写入 status=observed 成功，
`memory_context` dynamic 15 条里无锚点。

### 修复（`src/tools/profile.rs`）
- dynamic 主查询不动（handoff 本就不在其白名单）；新增 handoff 专查：取最新
  ≤3 条未取代（`superseded_by IS NULL`）、active、`valid_*` 生效的锚点**置顶**
  prepend 到 dynamic 列表。
- 渲染沿用 `derive_kind`：锚点以 `[handoff]` 标签出现在 prompt_block。
- 读侧回归（生产热态实测）：`memory_context` 现返回 `f8a2a042f4b4778b` 锚点置顶。

---

## 2026-10-28 — 交接协议 v1（跨智能体任务接力）

### 背景：无缝切换卖点的最后一公里
memoria 卖点「智能体无缝切换」实测卡在接入侧：客户端各自为战、无统一开场/收尾行为。
结论：协议应住服务端——MCP `initialize.instructions` + 工具描述两个通道都由服务端下发，
改协议全端生效，客户端零改动。

### 改动（`src/mcp_server.rs`，+26 行）
- **initialize 注入 `SERVER_INSTRUCTIONS`**：开场调 `memory_context` 拉交接状态（上一个
  会话/智能体刚定了什么、卡在哪、下一步）、会话中用 `memory_search` 查背景、轮末用
  `memory_observe` 写三段式交接记录（`[定了什么][卡在哪][下一步]`，source 带 agent 标识）。
- **`memory_context` / `memory_observe` 描述升级**：内嵌行为指令（"【会话开场必须先调我】"/
  "【每轮结束有状态变化就调我】"），兜底不渲染 instructions 的客户端——工具描述每次
  请求必注入模型，是第二通道。
- 明确边界：只做任务级状态接力，不逐字复述对话。

### 线上验证（2026-10-28，工作机生产）
- 版本 `0.4.0-ge2c0a1f`；initialize 下发 instructions、双工具描述已升级、health ok
  （embed pass）、数据完好（12,762 条）、家端隧道（127.0.0.1:9003）同样取到协议。
- 首条狗粮交接记录 `f81c4fba` 已按三段式写入 `agent/xujiayan`。

### 顺带排雷（部署时发现）
- **agent-core 编排缺 `MEMORIA_HOST=0.0.0.0`**：P2 迁移（10-22）后 memoria 由 agent-core
  Windows 服务编排，`agent.toml`/`services.json` 均未设 HOST → 任何重启都会绑回默认
  127.0.0.1、断掉 Tailscale 远程访问（潜伏 bug，重启机必炸）。两处均已补 0.0.0.0
  （备份 `.bak-20261028`），经重启 agent-core 服务生效，现绑 `0.0.0.0:9003`。
- ssh 跑 `restart.ps1` 的进程会被会话作业回收（AGENTS.md 已有教训，再次实证）；
  生产重启走 schtasks `MemoriaStackBoot` 或 agent-core 服务。

### 待办
- ~~ZCode / WorkBuddy 真实会话验证 instructions 渲染（服务端已确认下发）~~
  **ZCode 侧已验证（2026-10-29）**：无头实弹测试（zcode -p）证实——①ZCode 0.17.0 的 MCP SDK
  Client 会解析并缓存 `initialize.instructions`（bundle 内 `getInstructions` 仅 SDK 定义、
  零调用点），**不注入模型系统提示**；②协议照常生效，走的是第二通道（工具描述
  【会话开场必须先调我】）+ AGENTS.md 双保险：新会话第一个工具调用即
  `memory_context`，最新锚点全文拉回。验证过程中顺带修复 ZCode 配置问题：memoria
  MCP 条目含 schema 外键 `transport`，被客户端严格 schema 静默丢弃（移除后恢复加载）。
  WorkBuddy 渲染仍待验（其 memoria 连接器装而未用，见下）。
- `memoria connect <client>` 一键接入（注册身份+发钥+按客户端格式生成配置）。
- ~~WorkBuddy memoria 连接器装而未用（工具快照零 memoria 工具），待修~~
  **已装通 + 验证完成（2026-10-29）**：工具快照（10-28 23:15 起）含 memoria 全套 63 工具
  （经 agent-core 网关合并面，`memory_context`/`memory_observe` 描述为带行为指令的新版）。
  无头实弹验证（codebuddy CLI + `--mcp-config` 直挂 :9003）：**读侧打通**——memory_context
  经 ToolSearch→DeferExecuteTool 成功执行，最新锚点 `f8a2a042f4b4778b` 全文拉回。
  两个通道性发现：①instructions 不渲染（与 ZCode 同款，源码定性：WB 运行时无 SDK
  instructions 消费点）；②**工具描述通道被延迟加载削弱**——WB 的 ToolSearch 初始只列
  `mcp__memoria__*` 工具名不带描述，【会话开场必须先调我】不在初始上下文，schema 按
  需加载后才可见。故 WB 里协议无法靠服务端通道自驱，开场接力需工作区侧提示
  （AGENTS.md/工作区规则）或客户端配置预载 memoria schema。

---

## 2026-10-28 — env 权威源切换 + 备份 E1 多目标/manifest（家机改动收口推送）

### ① guard/canary env 解析切换 `~/.svc-secrets`（R0 密钥归位收尾）
- `recall_guard.py` / `scripts/retrieval_canary.py`：env 定位顺序改为
  `MEMORIA_ENV_PATH` → `~/.svc-secrets/agent-core.env`（restart.ps1/托盘的权威源）→
  `memoria.env` → 旧项目内 `.env` 兜底（2026-10-19 已退役）。裸 shell 运行时不再静默
  找错配置。
- `MCP_URL` 支持 `MEMORIA_MCP_URL` 环境变量覆盖，默认仍 `:9003`。

### ② `daily_memoria_backup.py` E1：多目标 + sha256 manifest
- 覆盖面按 D9 分级（2026-10-19 拍板）扩到三系统：memoria（memoria.db / audit.db 必备）、
  agent-core（audit_events / checkpoints 必备；harness 建议；meta_evolution 可选；
  boot.db **备份但不恢复**——启动熔断锁存，恢复=恢复故障态）、
  dashboard（dashboard.db / event_bus.db 必备；harness_store 建议）。
- 全部成功后写 `manifest_<stamp>.json` + `latest_manifest.json`
  （sha256 / size / source_mtime / DB user_version / role / restore / 三系统源版本戳），
  供 `svc verify-backup` 校验。
- 失败产物即删（损坏副本不得混入备份目录被误当可用备份）；非必备库损坏仅隔离
  告警不阻塞，必备库缺失/失败即退出码 1。

### 其他
- `backups/`（本机 exe 版本备份，~32MB 二进制）加入 .gitignore，不入库。

---

## 2026-10-13 — 运维卫生 + R0 检索金丝雀（事故复盘第二轮）

### ① 实验启动器归位 + 正本启动自检
- **实验启动器归位**：`C:\services\memoria\` 根目录下 6 个 `start_memoria_*` 启动器（2026-08 遗留、
  未纳入 git）全部移入 `experiments/`。危害点：它们用的是**与生产同一个 exe 和同一个生产库**
  （`MEMORIA_DB_PATH = data\memoria.db`），却带着 `RERANK_ENABLED=1` + `RERANK_URL=:8777/rerank`
  （死端口）等实验配置——10-06 的提权 rogue 实例即由此而来。除一条已失效的 qclaw 路径外，
  无任何现行引用，移入子目录不影响现有流程。
- **正本 `restart.ps1` 启动自检**：起服务前断言 `MEMORIA_EMBEDDING_URL` 非空 + 嵌入端点 `/health`
  探活，**任一不过直接拒启**（exit 2），并打印生效配置（EMBEDDING_URL / RERANK_ENABLED / DB）
  便于事后归因"当时到底带了什么环境"。原则：宁可起不来，不要带病上线——10-02 / 10-13 两次事故
  都是"服务起来了、health 也 200，但语义通道是死的"且不落日志。逃生舱：`-SkipEmbedCheck`。

### ② R0 检索金丝雀（`scripts/retrieval_canary.py`）
双探针，补既有监控的两个盲区：
- **探针 A 读侧空结果**（10-13 事故型）：取 N 条库里确实存在的记忆，用原文片段查 MCP；
  **仅当 SQLite 侧有命中而 MCP 返回 0 条**才计为引擎层空结果——即事故当时的人工判定法
  （"库里有、接口不给"）。≥2 条（或有命中探针的 ≥50%）才报警，避免冷门查询误报。
- **探针 B 写侧覆盖**（本次故障型）：`memories` vs `memory_vectors` 缺口率 >2% 且**持续**
  >60min 才报警（状态文件记首次超阈时间，防补嵌/批量写入期间抖动误报）。
  现 `recall_guard` 用关键词查询打 FTS，而 FTS 覆盖恒为 100% → 对语义退化**完全全盲**。
- **解析契约**：`_results_of()` 双 schema 兼容（`results` 顶层 / `recall.results`）。
- 退出码 0/1/2（正常/告警/运行错误），告警落 `canary_alert.json`，可被 `memory_ops_status` / 巡检读取。
- **负向验证**：越界阈值强制触发 → exit 1 且 severity=warn；双 schema 三种结构解析均正确。
- 同轮把该教训落到 `recall_guard.py`——其 `recall_topk` 原为单 schema 解析，一并双 schema 化
  （改后回归：完整内容@5 95.0% / 关键词@5 65.0%，均高于基线，severity=ok）。

### 待办
- 金丝雀尚未挂调度（30 分钟一次）。挂法与 `recall_guard.py` 一致，需提权创建计划任务。
- `memoria_stack_launcher.py`（管 Memoria :9003 + A2A Bridge :9000）因换 exe 被停，
  恢复需提权创建计划任务，暂未恢复——:9000 目前无人看护（:9003 由托盘看门狗覆盖）。

---
## 2026-10-13 — 文档 ingestion 事后嵌（P0-1）

### 背景：检索故障双线诊断的写侧缺口
10-13 检索空结果事故（ZCODE 闭环：提权 rogue 实例）之外，并行诊断发现**写侧向量缺口**：生产库 9749 条记忆中 995 条（10.2%）无向量、语义召回完全不可见（仅 FTS 可捞），其中 602 条为文档 ingestion 产物（2026-09/10 的 `[文档]/[文档块]`，集中于 hot tier document）——ingest 路径调 `remember_with_dedup` 时 hnsw/query_cache 恒传 None，candidate_vector 结构上恒空。

### 改动
- **`document.rs` 事后嵌**：`ingest_bytes` / `ingest_plain_text` / `write_chunk_memories` 新增 `embed: Option<EmbedFn>` + `hnsw: Option<&HnswIndex>` 参数；manifest 与每个分块在 remember 返回 id 后嵌 body 文本 → `persist_and_index`（put_stored_vector + hnsw.add，与 MCP 写入侧共享 helper）。fail-open：回调缺失/返回 None（嵌入服务故障）= 降级不嵌，记忆照常写入可 FTS 召回，向量留待补嵌脚本。
- **嵌入桥下沉 lib**：`embed_query` / `block_on_embed` 从 mcp_server（bin 专享）迁至 `vector/embedding.rs`——web_api（lib）与 mcp_server（bin）共用同一 HTTP 嵌入桥，杜绝双路径复制漂移。`block_on_embed` 用临时 current-thread 运行时 block_on（dispatch/web 均在 spawn_blocking 同步上下文，与 block_on_rerank 同款范式）。
- **Web UI 上传路径打通**：`WebApiState` 增加 hnsw / embedding_url / http_client（main.rs 与 AppState 同源 clone）——此前上传的文档结构上永远不进 HNSW。
- **回归测试**：`ingest_persists_vectors_when_embed_provided`（manifest+每块落 memory_vectors + HNSW）、`ingest_degrades_without_embed`（fail-open 记忆照常）、`ingest_embed_fn_none_also_degrades`（单条故障不阻断）。
- **ops**：restart.ps1 显式注入 `MEMORIA_EMBEDDING_URL=8778`（exe 默认 8777 死端口，10-02 同款坑）+ `MEMORIA_RERANK_ENABLED=0`（10-13 事故处置，ZCODE）。

### 补嵌脚本（本 PR 提供，为独立运维动作）
- **`scripts/backfill_missing_vectors.py`**：清存量——跨全部 ns 的缺失向量记忆补嵌（含文档块），
  与 `rebuild_vectors.py`（指定 ns 活跃记忆全量重嵌，修污染向量）分工明确。沿用退化拒收整批
  重试 + 维度校验 + 抽样一致性安全闸 + 单事务写回。嵌全文不截断（与 P0-1 后写入侧一致，
  避免向量空间分裂）。跑完需重启服务让新向量进内存 HNSW。

### 线上执行结果（2026-10-13）
- 补嵌 1001 条，安全闸 20/20 一致性 >=0.9 通过，覆盖率 9766/9767 = **100.0%**
  （余 1 条为空内容记忆 `e3b0c442…`，脚本按 `length(content)>0` 正确排除）。
- 语义召回实证：改写问句「迁移到新机器后怎么保证监控还能正常告警？」命中 5 条，
  全部为此前无向量的文档块（2026-10-13_运维及迁移可靠性方案_v1.md 的分块）。
- 根因 B 复核：正本实例普通 `memory_remember` 写入记忆同时落向量 → 10-09~10-11 那批
  352 条缺失确系 rogue 实例嵌入通道环境问题，非写入路径缺陷。

### 待办（未含本 PR）
- **写侧覆盖金丝雀**：`memories` vs `memory_vectors` 缺口率 >2% 持续 1h 报警。现 recall_guard
  关键词查询打 FTS，对语义退化全盲——本次故障 10-09 间歇出现、10-13 才确诊，正是缺该探针。

---

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
