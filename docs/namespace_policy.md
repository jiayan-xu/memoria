# Namespace Policy

> All namespaces MUST be ASCII-only slugs. Chinese display names live in the agent registry only.

## Slug Dictionary (single source of truth)

| 部门 | slug | status |
|------|------|--------|
| 运行部 | `yunxing` | planned |
| 财务部 | `caiwu` | planned |
| 办公室 | `zonghe` | planned |
| 工程部 | `engineering` | active (keep as-is) |
| 固废科 | `gufei` | active (keep as-is) |
| 环保部 | `huanbao` | planned |
| 检修部 | `jianxiu` | planned |
| 采购部 | `caigou` | planned |
| 安办 | `anban` | planned |

## Namespace Patterns

| Level | Pattern | Example | Access |
|-------|---------|---------|--------|
| Personal | `agent/<agent_id>` | `agent/cs-pufa-2nd-thermal_yunxing_lisi` | Owner only |
| Dept shared | `org/cs-pufa-2nd-thermal/dept/<slug>/shared` | `.../dept/yunxing/shared` | Dept read; write = lead or whitelist |
| Company shared | `org/cs-pufa-2nd-thermal/shared` | | All registered agents read |

## Rules

1. No Chinese, spaces, or full-width characters in namespace paths.
2. Existing `dept/engineering` and `dept/gufei` namespaces are frozen — do NOT rename.
3. Display names (中文) live in `register_agent` `display_name` field only.
4. `allowed_ns` MUST NOT contain `*` or `org/test/*` (admin excepted).

## Permission Template

| Role | permission | allowed_ns |
|------|-----------|------------|
| Regular | `user` | `["agent/<self>", "org/cs-pufa-2nd-thermal/dept/<slug>/shared"]` |
| Dept lead | `read_write` | Same as regular + `dept/<slug>/*` (if engine supports wildcard) |
| Admin | `admin` | `["*"]` — never distributed |
