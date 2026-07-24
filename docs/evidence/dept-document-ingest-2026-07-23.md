# 部门共享文档入库（PDF/DOCX）— 2026-07-23

> 给后续助手：用户要的是 **Memoria 侧** Word/PDF 上传存档进 **部门共享 ns**；待办生成暂不做。

## 能力

| 入口 | 路径 | 说明 |
|---|---|---|
| Web | `POST /api/documents` multipart(`file`,`namespace`) | 抽文本 + 落盘 + 记忆分块 |
| Web | `GET /api/documents?namespace=` | 列清单（`memory_type=document` 且无 parent） |
| UI | `/app` →「文档」页 | 默认 ns=`org/cs-pufa-2nd-thermal/dept/gufei` |
| MCP | `ingest_document` | 已抽文本入库（无二进制） |

## 存储约定

- 二进制：`{data}/documents/{ns_safe}/{doc_id}/{filename}`（`MEMORIA_DOC_DIR` 可覆盖根）
- 记忆：`memory_type=document`，`category=document`，tags 含 `dept-share`
- 清单行 `parent_id=NULL`；分块挂 `parent_id=清单id`；`raw_ref` 指旁路路径
- 上限 20 MiB；扫描件 PDF（可提取字数 &lt;50）明确失败

## 权限

写入目标 ns 须 `check_ns_access` 通过。上传身份建议用持有该部门 ns 的 agent（或 admin/jarvis）。

## 代码

- `src/document.rs`
- `src/web_api.rs`（`/api/documents`）
- `src/mcp_server.rs`（`ingest_document`）
- `web/index.html` + `web/dashboard.js`

## ��֤��2026-07-23 ������άʵ����

- [x] cargo check --bin memoria-server ͨ��
- [x] release ��������� ops memoria-core/target/release/memoria-server.exe`n- [x] MCP ingest_document �� ns=org/.../dept/gufei status=ok
- [x] HTTP multipart DOCX upload �� 200��
aw_ref=documents/...`n- [x] GET /api/documents �ɼ��嵥
- [x] Web index.html/dashboard.js ��ͬ���� ops web/`n

- [x] Excel .xlsx ���ı���⣨calamine��+ agent-core/PFAiX ���У�2026-07-23��

