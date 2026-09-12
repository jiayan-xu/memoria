# Memoria MCP Tool Contract

> Version: 2026-09-12 (initial freeze)
> This document defines the stable tool interface. Breaking changes require a major version bump and dual-run period.

## Core Memory Tools

### memory_remember

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| namespace | string | ✓ | Target namespace |
| content | string | ✓ | Memory content |
| tags | array[string] | | Tags |
| importance | number | | 0-1 importance weight |

**Returns:** `{ "status": "remembered", "id": "<memory_id>" }`

### memory_recall / memory_search

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| namespace | string | ✓ | Source namespace |
| query | string | ✓ | Search query |
| limit | number | | Max results (default varies) |

**Returns:** `{ "results": [{ "id", "content", "score", "access_count", ... }] }`

### memory_context

Session-opening context injection. Same input pattern as memory_search.

### memory_export

Streaming JSONL export by namespace. Output format: one JSON object per line.

### memory_profile

Agent memory profile / summary.

## Agent Management

### register_agent

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| agent_id | string | ✓ | Unique ID (ASCII slug) |
| display_name | string | | Chinese display name (not used in ns) |
| namespace | string | ✓ | Comma-separated allowed_ns list |
| permission | string | ✓ | admin / read_write / user |

**Returns:** `{ "badge": { "agent_id", "badge_token", ... } }`

### agent_list

**Returns:** `{ "agents": [{ "agent_id", "display_name", "namespace", "permission", "registered_at" }] }`

## A2A Messaging

### a2a_send

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| to | string | ✓ | Target agent_id |
| content | string | ✓ | Message content |

### a2a_recv

Receive pending messages for current agent.

### a2a_delete

Delete a processed A2A message.

## Error Codes

| Code | Meaning | Common cause |
|------|---------|-------------|
| -32001 | Authentication failed | Wrong/missing X-Agent-Id or X-Agent-Key |
| -32602 | Invalid params | Missing required field, bad type |
| 429 | Rate limited | LLM provider quota exhausted (fail-closed) |

## Known Gaps (needs implementation)

- `memory_delete`: NOT implemented. Test/orphan data cannot be removed via MCP.
- Namespace wildcard matching (`dept/*` covering `dept/张三`): untested/unsupported.
