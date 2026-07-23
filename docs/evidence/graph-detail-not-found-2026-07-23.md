# 图谱点击「错误: Not Found」— 2026-07-23

## 现象
Memoria Web 知识图谱点击任意节点 → 弹窗 `错误: Not Found`。

## 根因
1. 前端 `openDetail` 调用 `GET /api/memories/{id}`。
2. 该 **path 动态段路由** 在现网（axum merge + auth `layer`）下：
   - 无鉴权时曾出现 401（路由能匹配到 middleware）
   - 有鉴权时返回 **空 body 404**，handler 不执行（连 `|| async { "PROBE_OK" }` 探针亦然）
3. 同进程 `GET /api/memories?id=` / list / graph **正常**。

## 处置（规避，非修 axum 根因）
- 单条 CRUD 改为 query：`GET|PUT|DELETE /api/memories?id=`
- 前端 `fetchMemoryById` / `saveMemory` / `deleteMemory` 同步走 `?id=`
- 运行镜像 `memoria/web/dashboard.js` 已同步；二进制来自 `memoria-open/target/release`

## 验证
- `GET /api/memories?id=<真实id>` → 200 + 正文
- 图谱点击应出详情（浏览器强刷 Ctrl+F5）

## 残留
`/{id}` 动态段路由未恢复；若需兼容旧客户端再查 axum middleware/path 交互。
