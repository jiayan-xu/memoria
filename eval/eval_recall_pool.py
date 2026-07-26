"""候选集召回率：量化 reranker 能救回多少（reranker 硬上限）。

只对 NS=agent/xujiayan 的「可达强相关邻居」当真值（与 eval_recall_corrected.py 同口径）。
对每个探针调用 memory_recall(query, max_results=POOL)，统计相关 target 是否落在 top-k 候选集。
- 若 target 在 top-100 但不在 top-10 → reranker 理论上可把它推到 top-10（收益空间）。
- 若 target 连 top-100 都不在 → reranker 无能为力（需更强嵌入/索引）。

用法：
    python eval_recall_pool.py            # 默认 POOL=100
    POOL=50 python eval_recall_pool.py    # 自定义候选池大小
依赖：MEMORIA_DB_PATH / MEMORIA_ENV_PATH（P0 去硬编码）。
"""
import sqlite3, json, urllib.request, time, os
import numpy as np

DB = os.environ.get("MEMORIA_DB_PATH", "data/memoria.db")
ENV_PATH = os.environ.get("MEMORIA_ENV_PATH", ".env")
MCP_URL = "http://127.0.0.1:9003/mcp"
NS = "agent/xujiayan"
POOL = int(os.environ.get("POOL", "100"))
KS = [1, 3, 5, 10, 20, 50, 100]


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


ADMIN_KEY = load_env(ENV_PATH).get("MEMORIA_ADMIN_KEY", "")


def recall_ids(query, max_results=POOL):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_recall",
                          "arguments": {"query": query, "namespace": NS, "max_results": max_results}}}
    req = json.dumps(payload).encode("utf-8")
    headers = {"Content-Type": "application/json", "X-Agent-Id": "admin", "X-Agent-Key": ADMIN_KEY}
    r = urllib.request.Request(MCP_URL, data=req, headers=headers)
    with urllib.request.urlopen(r, timeout=90) as resp:
        body = json.loads(resp.read().decode("utf-8"))
    data = json.loads(body["result"]["content"][0]["text"])
    return [x["memory_id"] for x in data.get("results", [])]


con = sqlite3.connect(DB)
cur = con.cursor()
cur.execute("SELECT id, superseded_by, valid_from, valid_to FROM memories")
meta = {mid: (sup, vf, vt) for mid, sup, vf, vt in cur.fetchall()}
NOW = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())


def reachable(mid):
    sup, vf, vt = meta.get(mid, (None, None, None))
    if sup is not None:
        return False
    if vf is not None and vf > NOW:
        return False
    if vt is not None and vt < NOW:
        return False
    return True


cur.execute("""SELECT r.source_id, r.target_id FROM memory_relations r
               JOIN memories m1 ON r.source_id=m1.id
               WHERE m1.namespace=? AND r.relation_type IN ('same_entity','updates')""", (NS,))
probes = {}
for s, t in cur.fetchall():
    if reachable(t):
        probes.setdefault(s, set()).add(t)
cur.execute("SELECT id, content FROM memories WHERE namespace=?", (NS,))
content_of = {mid: c for mid, c in cur.fetchall()}
con.close()

import random
random.seed(7)
pids = [p for p in probes if p in content_of]
if len(pids) > 120:
    pids = random.sample(pids, 120)

cnt = {k: 0 for k in KS}
med = []
in_pool = 0
n = 0
t0 = time.time()
for pid in pids:
    targets = probes[pid]
    try:
        got = [i for i in recall_ids(content_of[pid][:512], POOL) if i != pid]
    except Exception as e:
        print("  recall FAIL:", e)
        continue
    if any(t in got for t in targets):
        in_pool += 1
    for k in KS:
        if any(t in got[:k] for t in targets):
            cnt[k] += 1
    for rk, t in enumerate(got, 1):
        if t in targets:
            med.append(rk)
            break
    n += 1
    if n % 30 == 0:
        print(f"  ...{n}/{len(pids)} ({time.time()-t0:.0f}s)")

P = n
print(f"\n=== 候选集召回率（NS={NS}, POOL={POOL}, n={P}, reranker 硬上限基线）===")
print(f"相关 target 进入 top-{POOL} 候选集比例: {in_pool/P*100:.1f}%  (reranker 理论上限)")
print(f"{'k':<5}{'recall@k':>10}")
for k in KS:
    print(f"{k:<5}{cnt[k]/P*100:>9.1f}%")
if med:
    med.sort()
    print(f"中位最佳排名（候选池内）: {med[len(med)//2]}")
print("DONE")
