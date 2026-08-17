# 演进日志 / CHANGELOG

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