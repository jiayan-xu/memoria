#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""R0 检索金丝雀（双探针 + 双 schema 解析）——2026-10-13 事故复盘产物。

为什么需要它
------------
2026-10-13 的检索故障有两条线，两条线**都不在既有监控视野内**：
- 读侧：提权 rogue 实例从 10-09 起间歇、10-13 上午持续性返回空结果
  （SQLite 直查 22 条命中 vs MCP 返回 0），直到当天下午才确诊；
- 写侧：文档 ingestion 路径与 rogue 实例期的写入不落向量，累计 995 条
  （10.2%）记忆语义不可见——**而 recall_guard 用关键词查询打 FTS，
  FTS 覆盖 100%，对语义退化完全全盲**，护栏读数全程"正常"。

本金丝雀就是这个窟窿的补丁，每次运行（建议 30 分钟一次）跑两个探针：

探针 A —— 读侧空结果（10-13 事故型）
    取 N 条"库里确实存在"的记忆，用其原文片段查 MCP；
    **仅当 SQLite 侧有命中、而 MCP 返回 0 条**才计为引擎层空结果
    （正是事故当时的人工判定法：库里有、接口不给）。
    单独一条 miss 可能是查询本身冷门；≥2 条（或有命中探针的 ≥50%）才报警。

探针 B —— 写侧覆盖（本次故障型）
    `memories` vs `memory_vectors` 行数差 / 总数 = 缺口率。
    超过阈值（默认 2%）且**持续**超过 N 分钟（默认 60）才报警——
    用状态文件记首次超阈时间，避免补嵌/大批量写入过程中的抖动误报。

解析契约（务必保持）
--------------------
`results` 字段曾在构建间从 `recall.results` 挪到顶层。ZCODE 复盘教训：
单 schema 解析会把"服务器正常返回 fused=5"误读成"全空"，把诊断方向
带偏成"环境毒化"。故一律走 `_results_of()` 双兼容解析，新增探针亦然。

退出码
------
0 = 正常 / 1 = 告警（任一探针触发）/ 2 = 运行错误（连不上服务、库无数据）

用法
----
    python scripts/retrieval_canary.py                # 跑一次，落告警文件
    python scripts/retrieval_canary.py --json-out x.json
    python scripts/retrieval_canary.py --gap-threshold 0.02 --sustain-min 60
    python scripts/retrieval_canary.py --ns agent/xujiayan
"""
import argparse
import json
import os
import sqlite3
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
DB = os.environ.get(
    "MEMORIA_DB_PATH",
    os.path.join(HERE, "..", "..", "memoria", "data", "memoria.db"),
)
MCP_URL = "http://127.0.0.1:9003/mcp"
NS = os.environ.get("MEMORIA_GUARD_NS", "agent/xujiayan")

# 探针 A
PROBE_N = int(os.environ.get("CANARY_PROBE_N", "6"))
EMPTY_FAIL_MIN = int(os.environ.get("CANARY_EMPTY_FAIL_MIN", "2"))
# 探针 B
GAP_THRESHOLD = float(os.environ.get("CANARY_GAP_THRESHOLD", "0.02"))
SUSTAIN_MIN = float(os.environ.get("CANARY_SUSTAIN_MIN", "60"))

ALERT = os.environ.get("CANARY_ALERT_JSON",
                       os.path.join(HERE, "canary_alert.json"))
STATE = os.environ.get("CANARY_STATE_JSON",
                       os.path.join(HERE, "canary_state.json"))
TIMEOUT = 30  # 单条 MCP 查询超时（金丝雀要快，不作长尾等待）


def resolve_env_path():
    """定位运行时 .env（与 recall_guard.py 同款，避免硬编码用户名路径）。"""
    if os.environ.get("MEMORIA_ENV_PATH"):
        return os.environ["MEMORIA_ENV_PATH"]
    for cand in (os.path.join(HERE, "..", "..", "memoria", ".env"),
                 os.path.join(HERE, "..", "memoria", ".env"),
                 os.path.join(HERE, ".env")):
        if os.path.exists(cand):
            return cand
    return os.path.join(HERE, ".env")


def load_env(p):
    d = {}
    try:
        for line in open(p, encoding="utf-8"):
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            d[k.strip()] = v.strip().strip('"').strip("'")
    except FileNotFoundError:
        pass
    return d


def _results_of(inner):
    """双 schema 兼容解析 results。

    历史：`results` 曾挂在 `recall` 子对象下，后挪到顶层。单 schema 解析在
    跨构建/跨版本调用时会把"服务器正常返回"误判为"空结果"（ZCODE 2026-10-13
    复盘的核心教训）。这里两级都试，空则取 []。
    """
    if not isinstance(inner, dict):
        return []
    r = inner.get("results")
    if r is None:
        rec = inner.get("recall")
        if isinstance(rec, dict):
            r = rec.get("results")
    return r if isinstance(r, list) else []


def mcp_search(query, ns, admin_key, k=5):
    """调 memory_search，返回 (ok, results)。ok=False 表示调用层出错。"""
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_search",
                          "arguments": {"query": query, "namespace": ns,
                                        "max_results": k}}}
    req = urllib.request.Request(
        MCP_URL, data=json.dumps(payload).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "X-Agent-Id": "admin", "X-Agent-Key": admin_key})
    try:
        d = json.loads(urllib.request.urlopen(req, timeout=TIMEOUT).read())
        txt = "".join(c.get("text", "")
                      for c in (d.get("result") or {}).get("content", []))
        inner = json.loads(txt) if txt.strip().startswith(("{", "[")) else {}
        return True, _results_of(inner)
    except Exception as e:  # noqa: BLE001 — 金丝雀不应因单次异常中断
        return False, str(e)


def probe_read_side(ns, admin_key, n=PROBE_N):
    """探针 A：库里有、接口不给 = 引擎层空结果。

    返回 (engine_empties, checked, detail)
    """
    con = sqlite3.connect(DB, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    cur = con.cursor()
    # 确定性取样：id 是内容 sha256，稳定不变（除非记忆被删），
    # 使历次运行的探针集可比。
    cur.execute(
        """SELECT id, content FROM memories
            WHERE namespace=? AND length(coalesce(content,''))>20
              AND superseded_by IS NULL
              AND (valid_to IS NULL OR valid_to='' OR valid_to>datetime('now'))
            ORDER BY id LIMIT ?""", (ns, n))
    rows = cur.fetchall()

    engine_empties = 0
    checked = 0
    detail = []
    for mid, content in rows:
        frag = " ".join((content or "").split())[:80]
        if not frag:
            continue
        # SQLite 侧：确认这段内容确实存在于库中（LIKE 前缀片段）
        like = "%" + frag[:20].replace("%", "\\%").replace("_", "\\_") + "%"
        db_hits = cur.execute(
            "SELECT COUNT(*) FROM memories WHERE namespace=? AND content LIKE ? "
            "ESCAPE '\\'", (ns, like)).fetchone()[0]
        ok, res = mcp_search(frag, ns, admin_key)
        got = len(res) if ok else -1
        if not ok:
            detail.append({"id": mid[:12], "db_hits": db_hits,
                           "mcp": "error", "error": res})
            continue
        checked += 1
        is_engine_empty = (db_hits > 0 and got == 0)
        if is_engine_empty:
            engine_empties += 1
        detail.append({"id": mid[:12], "db_hits": db_hits, "mcp_hits": got,
                       "engine_empty": is_engine_empty})
    con.close()
    return engine_empties, checked, detail


def probe_write_side():
    """探针 B：向量覆盖缺口率 + 持续时长。返回 (gap_ratio, missing, total, sustained_min)。"""
    con = sqlite3.connect(DB, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    total = con.execute("SELECT COUNT(*) FROM memories").fetchone()[0]
    have = con.execute("SELECT COUNT(*) FROM memory_vectors").fetchone()[0]
    con.close()
    missing = max(0, total - have)
    gap = (missing / total) if total else 0.0

    state = {}
    if os.path.exists(STATE):
        try:
            state = json.load(open(STATE, encoding="utf-8"))
        except Exception:  # noqa: BLE001
            state = {}
    now = time.time()
    if gap > GAP_THRESHOLD:
        first = state.get("gap_first_seen_ts", now)
    else:
        first = None
    try:
        json.dump({"gap_first_seen_ts": first, "last_gap": gap,
                   "last_check_ts": now},
                  open(STATE, "w", encoding="utf-8"), ensure_ascii=False, indent=2)
    except Exception:  # noqa: BLE001 — 状态文件写失败不应掩盖探针结论
        pass
    sustained_min = ((now - first) / 60.0) if first else 0.0
    return gap, missing, total, sustained_min


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ns", default=NS, help="主探针命名空间")
    ap.add_argument("--probes", type=int, default=PROBE_N, help="探针 A 取样条数")
    ap.add_argument("--gap-threshold", type=float, default=GAP_THRESHOLD)
    ap.add_argument("--sustain-min", type=float, default=SUSTAIN_MIN)
    ap.add_argument("--json-out", default=None, help="另存明细 JSON 路径")
    args = ap.parse_args()

    admin_key = load_env(resolve_env_path()).get("MEMORIA_ADMIN_KEY", "")
    if not admin_key:
        print("❌ 未取到 MEMORIA_ADMIN_KEY（检查 MEMORIA_ENV_PATH / .env）")
        sys.exit(2)

    if not os.path.exists(DB):
        print(f"❌ 生产库不存在: {DB}")
        sys.exit(2)

    # ── 探针 A ──
    empties, checked, detail = probe_read_side(args.ns, admin_key, args.probes)
    fail_a = (checked == 0) or (empties >= max(EMPTY_FAIL_MIN, checked * 0.5))

    # ── 探针 B ──
    gap, missing, total, sustained = probe_write_side()
    fail_b = (gap > args.gap_threshold) and (sustained >= args.sustain_min)

    severity = "ok"
    if fail_a and fail_b:
        severity = "critical"
    elif fail_a or fail_b:
        severity = "warn"

    alert = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "severity": severity,
        "probe_a_read": {
            "ns": args.ns, "checked": checked, "engine_empties": empties,
            "failed": bool(fail_a), "detail": detail,
        },
        "probe_b_write": {
            "memories": total, "memory_vectors": total - missing,
            "missing": missing, "gap_ratio": round(gap, 4),
            "threshold": args.gap_threshold,
            "sustained_min": round(sustained, 1),
            "threshold_min": args.sustain_min, "failed": bool(fail_b),
        },
    }
    for path in ({ALERT, args.json_out} - {None}):
        try:
            json.dump(alert, open(path, "w", encoding="utf-8"),
                      ensure_ascii=False, indent=2)
        except Exception as e:  # noqa: BLE001
            print(f"[canary] 告警文件写入失败 {path}: {e}")

    print(f"[canary] 探针A 读侧: 检查 {checked} 条, 引擎层空结果 {empties} 条 "
          f"→ {'❌ 异常' if fail_a else '✅ 正常'}")
    print(f"[canary] 探针B 写侧: 缺口 {missing}/{total} = {gap * 100:.1f}% "
          f"(阈值 {args.gap_threshold * 100:.0f}%), 持续 {sustained:.0f} 分钟 "
          f"→ {'❌ 异常' if fail_b else '✅ 正常'}")
    print(f"[canary] severity={severity}（告警文件 {ALERT}）")

    sys.exit(1 if (fail_a or fail_b) else 0)


if __name__ == "__main__":
    main()
