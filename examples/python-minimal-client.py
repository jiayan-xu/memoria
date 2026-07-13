#!/usr/bin/env python3
"""最小 Memoria MCP HTTP 客户端示例（P2-6 DX）。

演示如何向 Memoria 发送一次 JSON-RPC `tools/call`（memory_remember）。
无密钥硬编码：AGENT_KEY 通过环境变量注入，占位符仅作文档用途。

Minimal Memoria MCP-over-HTTP client. No hardcoded secrets:
AGENT_KEY comes from the environment, the placeholder is documentation-only.

依赖 / Requires: Python 3.8+ (标准库，无第三方依赖 / stdlib only)
运行 / Run:
    export MEMORIA_URL=http://127.0.0.1:9003/mcp
    export AGENT_ID=example-client
    export AGENT_KEY=<your-agent-key>
    python3 examples/python-minimal-client.py
"""
import json
import os
import urllib.request

MEMORIA_URL = os.getenv("MEMORIA_URL", "http://127.0.0.1:9003/mcp")
AGENT_ID = os.getenv("AGENT_ID", "example-client")
# 占位符 <AGENT_KEY> 仅作文档；真实值务必从环境变量读取，切勿硬编码。
AGENT_KEY = os.getenv("AGENT_KEY", "<AGENT_KEY>")


def mcp_call(method: str, params: dict, msg_id: int = 1) -> dict:
    """发送一个 JSON-RPC 请求到 Memoria MCP 端点。"""
    payload = {"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params}
    req = urllib.request.Request(
        MEMORIA_URL,
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "X-Agent-Id": AGENT_ID,
            "X-Agent-Key": AGENT_KEY,
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        body = resp.read().decode("utf-8")
    # Memoria 对 tools/call 返回标准 JSON-RPC；SSE 流则需按行解析，此处示例按 JSON。
    return json.loads(body)


if __name__ == "__main__":
    result = mcp_call(
        "tools/call",
        {
            "name": "memory_remember",
            "arguments": {
                "content": "用户偏好用中文交流。",
                "role": "user",
                "namespace": "example",
            },
        },
    )
    print(json.dumps(result, ensure_ascii=False, indent=2))
