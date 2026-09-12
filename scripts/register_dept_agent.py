#!/usr/bin/env python
"""部门 agent 注册脚本 — 生成 register_agent 参数并验证 ns 合法性。

用法：
  python scripts/register_dept_agent.py --name 李四 --dept yunxing --role regular
  python scripts/register_dept_agent.py --name 张三 --dept engineering --role lead
  python scripts/register_dept_agent.py --name 李四 --dept yunxing --validate-only

输出：可直接用于 memoria MCP register_agent 的 JSON 参数。
"""
from __future__ import annotations

import argparse
import json
import re
import sys

# 单一权威来源：docs/namespace_policy.md slug 字典
SLUGS = {
    "yunxing": "运行部", "caiwu": "财务部", "zonghe": "办公室",
    "engineering": "工程部", "gufei": "固废科", "huanbao": "环保部",
    "jianxiu": "检修部", "caigou": "采购部", "anban": "安办",
}

NS_RE = re.compile(r"^[a-z0-9_\-/.]+$", re.ASCII)
COMPANY = "org/cs-pufa-2nd-thermal"


def make_agent_id(name: str, dept: str) -> str:
    """agent_id 格式：cs-pufa-2nd-thermal_<dept>_<拼音或工号>"""
    safe = re.sub(r"[^a-z0-9_]", "", name.lower().replace(" ", "_"))
    return f"cs-pufa-2nd-thermal_{dept}_{safe}" if safe else f"cs-pufa-2nd-thermal_{dept}_unnamed"


def validate_ns(ns: str) -> list[str]:
    errors = []
    if not NS_RE.match(ns):
        errors.append(f"ns 含非法字符（仅允许 a-z 0-9 _ - / .）: {ns}")
    for seg in ns.split("/"):
        if re.search(r"[\u4e00-\u9fff]", seg):
            errors.append(f"ns 路径段含中文: '{seg}'")
        if " " in seg:
            errors.append(f"ns 路径段含空格: '{seg}'")
    return errors


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", required=True, help="同事姓名（中文，用于 display_name）")
    ap.add_argument("--dept", required=True, choices=list(SLUGS), help="部门 slug")
    ap.add_argument("--role", default="regular", choices=["regular", "lead"], help="权限角色")
    ap.add_argument("--validate-only", action="store_true", help="只校验不输出")
    args = ap.parse_args()

    if args.dept not in SLUGS:
        print(f"错误：未知部门 slug '{args.dept}'，可选：{', '.join(SLUGS)}")
        return 1

    agent_id = make_agent_id(args.name, args.dept)
    personal_ns = f"agent/{agent_id}"
    dept_shared = f"{COMPANY}/dept/{args.dept}/shared"

    allowed_ns = [personal_ns, dept_shared]
    if args.role == "lead":
        dept_wild = f"{COMPANY}/dept/{args.dept}/*"
        allowed_ns.append(dept_wild)

    # 校验所有 ns 均为 ASCII
    all_errors = []
    for ns in allowed_ns:
        all_errors.extend(validate_ns(ns))
    if all_errors:
        for e in all_errors:
            print(f"✗ {e}", file=sys.stderr)
        return 1

    display_name = f"{args.name}（{SLUGS[args.dept]}）"
    permission = "read_write" if args.role == "lead" else "user"

    params = {
        "agent_id": agent_id,
        "display_name": display_name,
        "namespace": ", ".join(allowed_ns),
        "permission": permission,
    }

    if args.validate_only:
        print(f"✓ 校验通过")
        print(f"  agent_id:   {agent_id}")
        print(f"  display:    {display_name}")
        print(f"  permission: {permission}")
        print(f"  allowed_ns: {allowed_ns}")
        return 0

    print(json.dumps(params, ensure_ascii=False, indent=2))
    print(f"\n# 使用：将以上 JSON 作为 memoria MCP register_agent 的 arguments 调用")
    return 0


if __name__ == "__main__":
    import json
    sys.exit(main())
