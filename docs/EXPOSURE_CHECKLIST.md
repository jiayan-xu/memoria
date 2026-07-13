# Memoria 公网暴露扫描清单（P0-2）

> 目标：默认最小暴露面，远程访问必须走反代 + TLS + 认证。本清单用于上线前逐项核对。

## 1. 监听地址（默认 loopback）

- `MEMORIA_HOST` 默认 `127.0.0.1`（仅本机可达）。
- 显式设 `MEMORIA_HOST=0.0.0.0`（或 `::`）时，启动会打印 **[WARN]** 提示暴露公网——这是允许的逃生通道，但**必须**配合下方反代。
- ✅ 上线前确认：未设置 `MEMORIA_HOST` 或仅 `127.0.0.1`。

## 2. 远程访问规范（不要直连 9003）

需要跨机访问时：

1. 前端/网关（如 bridge `:9000`、dashboard `:8000`、PFAiX）与 memoria `:9003` 之间**只走本机或内网**。
2. 对外唯一入口走**反向代理**（nginx / caddy / 云 LB），强制 **TLS**。
3. 代理层做**认证**（转发 `X-Agent-Id` / `X-Agent-Key`，或网关层 bearer）。
4. memoria 自身**不**直接监听 `0.0.0.0` 对公网开放。

## 3. 端点暴露矩阵

| 端点 | 端口 | 鉴权 | 说明 |
|------|------|------|------|
| `GET /health` | 9003 | 无（公开精简） | 仅返回 `{status, service, version}`，无内部细节 |
| `GET /health/full` | 9003 | **需 admin** | `X-Agent-Id/Key`（admin 角色）或 `x-admin-key` 头；否则 403 |
| `POST /mcp` | 9003 | **需认证** | MCP 协议，所有工具经 `X-Agent-Id/Key` + NS 检查 |
| `MCP memory_health` 工具 | 9003 | **需 admin** | 完整健康报告，admin 角色或 `admin_key` 兜底 |
| `GET /api/*`（`/stats` `/graph` `/decay_timeline` `/api/memories` `/api/relations`） | 9003 | **需认证** | `web_api` 全局 `auth_middleware` |
| `MEMORIA_DB_PATH` / `admin_key.secret` | 本地文件 | 文件权限 | 见第 4 节 |

- ❌ 不应存在：任何无鉴权的 `/health/full`、DB 文件直接 HTTP 暴露、admin key 回显到日志。

## 4. 密钥与凭据

- `MEMORIA_ADMIN_KEY`：**必须显式设置且非空**（写入 `.env`，`.env` 入 `.gitignore`，**绝不提交仓库**）。未设置或为空时进程**拒绝启动**（不再使用可预测的 timestamp 自动 key）。
- 轮换 admin key 后，需同步更新依赖它的服务（agent-core `MEMORIA_ADMIN_KEY`、PFAiX、dashboard）配置。

## 5. 端口速查（本机栈）

| 服务 | 端口 | 角色 |
|------|------|------|
| memoria | 9003 | 记忆层 MCP 服务（本任务） |
| bridge | 9000 | MCP 网关 |
| dashboard | 8000 | 运维 UI |
| agent-core | 9753 | Agent 运行时 |

> 扫描公网时，这些端口**不应**在防火墙/安全组对 `0.0.0.0` 开放；仅 `127.0.0.1` 或受信任内网 CIDR 可达。

## 6. 上线前核对（checklist）

- [ ] `MEMORIA_HOST` 未设为 `0.0.0.0` / `::`（或已配反代 + TLS + 认证）
- [ ] `MEMORIA_ADMIN_KEY` 已显式设置且不在 git 历史中
- [ ] `.env` 在 `.gitignore`
- [ ] 防火墙/安全组未对公网开放 9003/9000/8000/9753
- [ ] `curl http://<host>:9003/health/full` 无 key 返回 403
- [ ] `curl http://<host>:9003/health` 不含内部细节
