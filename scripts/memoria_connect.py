#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""memoria connect —— 智能体接入 memoria（③④件：MCP 配置 + AGENTS.md 接力规则块）。

分工：①注册身份 ②发钥 走现成 MCP 渠道（register_agent 工具，admin 身份调用，
参数 admin_key/agent_id/display_name/namespace，返回中取口令）；本脚本完成
③按客户端格式写 MCP 配置（zcode / claude-code / generic）与
④工作区 AGENTS.md 接力规则块（幂等标记合并，重复接入不重复写）。

用法（①②完成后）：
  python memoria_connect.py --client zcode --agent-id myagent \\
      --agent-key <取得的key> --workspace C:\\path\\to\\workspace

安全：memoria URL 仅允许 http/https；日志只落 key 指纹（sha256 前 12 位）；
全部文件写入为原子替换（tmp + os.replace）。
"""
import argparse
import hashlib
import io
import json
import os
import re
import sys
import tempfile
import urllib.request

MARK_BEGIN = "<!-- memoria-connect begin -->"
MARK_END = "<!-- memoria-connect end -->"

RULES_BLOCK = MARK_BEGIN + """
## memoria 跨会话记忆接力（由 memoria connect 生成，勿删标记）

memoria 是本机跨客户端持久记忆库（ZCode / WorkBuddy / 其他 agent 共享）。

1. **用户提到「刚刚 / 之前 / 上一轮 / 我和 zcode / 我和 workbuddy / 别的会话」聊过的事**时，
   第一动作是查 memoria，不要凭空回答"不知道"或"会话相互隔离"：
   - 开放查询用 `memory_search`（namespace=`{namespace}`，用用户话里的关键词）；
   - 要最新任务状态/交接锚点用 `memory_context`（namespace=`{namespace}`）。
2. 检索命中后按最新记录（created_at 最新）作答；如内容含「[下一步]」，顺着它接续工作。
3. 本轮产生了新决定/新进展/新卡点时，轮末用 `memory_observe`（同 namespace，source 写你的
   agent 标识）写一条三段式：[定了什么] … [卡在哪] … [下一步] …，让下一个会话能接上。
4. memoria 不可达时（MCP 报错/超时）直接说明，不要阻塞主线。
""" + MARK_END


def fp(tok):
    return hashlib.sha256(tok.encode()).hexdigest()[:12] if tok else "None"


def log(msg):
    print("[connect] " + msg)


def die(msg, code=2):
    print("[connect] FAIL: " + msg, file=sys.stderr)
    sys.exit(code)


def load_admin_key(explicit):
    if explicit:
        return explicit
    if os.environ.get("MEMORIA_ADMIN_KEY"):
        return os.environ["MEMORIA_ADMIN_KEY"]
    env_path = os.path.join(os.path.expanduser("~"), ".svc-secrets", "agent-core.env")
    if os.path.exists(env_path):
        m = re.search(r"^MEMORIA_ADMIN_KEY=(.+)$",
                      io.open(env_path, encoding="utf-8").read(), re.M)
        if m:
            return m.group(1).strip().strip('"')
    return None


def check_url(url):
    if not re.match(r"^https?://[A-Za-z0-9._\-]+(:\d+)?(/|$)", url):
        die("memoria URL 仅允许 http/https: " + url)
    return url


def atomic_write(path, text):
    d = os.path.dirname(os.path.abspath(path)) or "."
    fd, tmp = tempfile.mkstemp(prefix=".connect-", dir=d)
    with os.fdopen(fd, "w", encoding="utf-8", newline="") as f:
        f.write(text)
    os.replace(tmp, path)


def write_mcp_config(client, url, agent_id, agent_key):
    entry = {"type": "http", "url": url,
             "headers": {"X-Agent-Id": agent_id, "X-Agent-Key": agent_key}}
    if client == "zcode":
        path = os.path.join(os.path.expanduser("~"), ".zcode", "cli", "config.json")
        cfg = {}
        if os.path.exists(path):
            cfg = json.load(io.open(path, encoding="utf-8"))
        cfg.setdefault("mcp", {}).setdefault("servers", {})["memoria"] = entry
        atomic_write(path, json.dumps(cfg, ensure_ascii=False, indent=2) + "\n")
        log("③ MCP 配置已写入 " + path + "（mcp.servers.memoria）")
        return path
    if client == "claude-code":
        path = os.path.join(os.path.expanduser("~"), ".claude.json")
        cfg = {}
        if os.path.exists(path):
            cfg = json.load(io.open(path, encoding="utf-8"))
        cfg.setdefault("mcpServers", {})["memoria"] = entry
        atomic_write(path, json.dumps(cfg, ensure_ascii=False, indent=2) + "\n")
        log("③ MCP 配置已写入 " + path + "（mcpServers.memoria）")
        return path
    log("③ generic 模式：请把以下配置贴入客户端的 MCP 设置（服务器名 memoria）：")
    print(json.dumps({"memoria": entry}, ensure_ascii=False, indent=2))
    return None


def merge_agents_md(workspace, namespace):
    path = os.path.join(workspace, "AGENTS.md")
    block = RULES_BLOCK.format(namespace=namespace)
    if os.path.exists(path):
        raw = io.open(path, encoding="utf-8").read()
        pat = re.compile(re.escape(MARK_BEGIN) + r".*?" + re.escape(MARK_END), re.S)
        if pat.search(raw):
            raw = pat.sub(lambda _: block, raw)
            action = "已替换既有规则块"
        else:
            raw = raw.rstrip("\n") + "\n\n" + block + "\n"
            action = "已追加规则块"
    else:
        raw = "# 工作区协作规则（AGENTS.md）\n\n" + block + "\n"
        action = "已新建"
    atomic_write(path, raw)
    log("④ AGENTS.md " + action + " -> " + path)
    return path


def main():
    ap = argparse.ArgumentParser(
        prog="memoria_connect",
        description="接入 memoria ③④：写 MCP 配置 + AGENTS.md 接力规则块。"
                    "①②注册发钥走现成 MCP 渠道（register_agent 工具，见 docstring）。",
        epilog="①②注册示例（任意已接入客户端或 curl 调 memoria MCP 的 register_agent，"
               "参数 admin_key/agent_id/display_name/namespace）："
               "成功后取得 agent key，再跑本脚本完成③④。")
    ap.add_argument("--client", required=True,
                    choices=["zcode", "claude-code", "workbuddy", "generic"])
    ap.add_argument("--agent-id", required=True)
    ap.add_argument("--agent-key", required=True)
    ap.add_argument("--namespace", default="agent/xujiayan")
    ap.add_argument("--workspace", default=os.getcwd())
    ap.add_argument("--memoria-url", default="http://127.0.0.1:9003")
    ap.add_argument("--skip-agents-md", action="store_true",
                    help="只写 MCP 配置，不动工作区 AGENTS.md")
    ap.add_argument("--dry-run", action="store_true", help="只打印计划，不写任何文件")
    a = ap.parse_args()

    if not re.match(r"^https?://[A-Za-z0-9._\-]+(:\d+)?(/|$)", a.memoria_url):
        die("memoria URL 仅允许 http/https: " + a.memoria_url)
    log("客户端=%s 身份=%s ns=%s url=%s workspace=%s key_fp=%s%s"
        % (a.client, a.agent_id, a.namespace, a.memoria_url, a.workspace,
           fp(a.agent_key), " [dry-run]" if a.dry_run else ""))
    if a.dry_run:
        log("[dry-run] 将写 MCP 配置（%s）%s"
            % (a.client, "" if a.skip_agents_md
               else "与 " + os.path.join(a.workspace, "AGENTS.md")))
        return 0
    write_mcp_config(a.client, a.memoria_url, a.agent_id, a.agent_key)
    if not a.skip_agents_md:
        merge_agents_md(a.workspace, a.namespace)
    log("完成。新会话即具备跨会话接力能力。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
