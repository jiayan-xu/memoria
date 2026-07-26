"""修正口径后的真实召回率：只用「可达(reachable)」强相关邻居当真值。

可达 = superseded_by IS NULL 且 valid_from<=now<=valid_to（与 hybrid.rs is_latest_now 一致）。
对每个探针：纯余弦上限(离线) vs A+C 管线(memory_recall, max_results=15)，同集对比 recall@k。
"""
import sqlite3, json, urllib.request, time, array, os
import numpy as np

# P0: 禁止硬编码本机用户名绝对路径。DB 经 MEMORIA_DB_PATH 注入，.env 路径经 MEMORIA_ENV_PATH 注入，
# 均回退到相对路径（脚本应在仓库根或 memoria 运行目录附近执行）。
DB = os.environ.get("MEMORIA_DB_PATH", "data/memoria.db")
ENV_PATH = os.environ.get("MEMORIA_ENV_PATH", ".env")
MCP_URL = "http://127.0.0.1:9003/mcp"
NS = "agent/xujiayan"
KS = [1, 3, 5, 10]

def load_env(p):
    d = {}
    for line in open(p, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        d[k.strip()] = v.strip().strip('"').strip("'")
    return d

ADMIN_KEY = load_env(ENV_PATH)["MEMORIA_ADMIN_KEY"]

def recall_ids(query, max_results=15):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_recall",
                          "arguments": {"query": query, "namespace": NS, "max_results": max_results}}}
    req = json.dumps(payload).encode("utf-8")
    headers = {"Content-Type": "application/json", "X-Agent-Id": "admin", "X-Agent-Key": ADMIN_KEY}
    r = urllib.request.Request(MCP_URL, data=req, headers=headers)
    with urllib.request.urlopen(r, timeout=60) as resp:
        body = json.loads(resp.read().decode("utf-8"))
    data = json.loads(body["result"]["content"][0]["text"])
    return [x["memory_id"] for x in data.get("results", [])]

con = sqlite3.connect(DB)
cur = con.cursor()
cur.execute("SELECT v.id, v.vector FROM memory_vectors v")
ids, vecs = [], []
for mid, blob in cur.fetchall():
    a = array.array("f"); a.frombytes(blob); ids.append(mid); vecs.append(list(a))
mat = np.array(vecs, dtype=np.float32)
norms = np.linalg.norm(mat, axis=1, keepdims=True); norms[norms == 0] = 1.0
mat = mat / norms
idx_of = {mid: i for i, mid in enumerate(ids)}

cur.execute("SELECT id, superseded_by, valid_from, valid_to FROM memories")
meta = {mid: (sup, vf, vt) for mid, sup, vf, vt in cur.fetchall()}
NOW = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
def reachable(mid):
    sup, vf, vt = meta.get(mid, (None, None, None))
    if sup is not None: return False
    if vf is not None and vf > NOW: return False
    if vt is not None and vt < NOW: return False
    return True

# 强相关边 → 只保留可达 target
cur.execute("""SELECT r.source_id, r.target_id FROM memory_relations r
               JOIN memories m1 ON r.source_id=m1.id
               WHERE m1.namespace=? AND r.relation_type IN ('same_entity','updates')""", (NS,))
probes = {}
for s, t in cur.fetchall():
    if reachable(t):
        probes.setdefault(s, set()).add(t)
cur.execute("SELECT id, content FROM memories WHERE namespace=?", (NS,))
content_of = {mid: c for mid, c in cur.fetchall()}

import random
random.seed(7)
pids = [p for p in probes if p in content_of and p in idx_of]
if len(pids) > 120:
    pids = random.sample(pids, 120)

off = {k: 0 for k in KS}
liv = {k: 0 for k in KS}
off_med = []; liv_med = []
t0 = time.time(); n = 0
for pid in pids:
    targets = probes[pid]
    # 离线纯余弦
    qi = idx_of[pid]
    sims = mat @ mat[qi]
    order = np.argsort(-sims)
    top = [ids[int(i)] for i in order if int(i) != qi][:15]
    for k in KS:
        if any(t in top[:k] for t in targets): off[k] += 1
    for rk, t in enumerate(top, 1):
        if t in targets: off_med.append(rk); break
    # A+C 管线
    try:
        got = [i for i in recall_ids(content_of[pid][:512], 15) if i != pid][:15]
    except Exception as e:
        continue
    for k in KS:
        if any(t in got[:k] for t in targets): liv[k] += 1
    for rk, t in enumerate(got, 1):
        if t in targets: liv_med.append(rk); break
    n += 1
    if n % 30 == 0: print(f"  ...{n}/{len(pids)} ({time.time()-t0:.0f}s)")

P = len(pids)
print(f"\n=== 修正口径真实召回率（可达强相关邻居, NS={NS}, n={P}）===")
print(f"{'k':<4}{'纯余弦上限':>12}{'A+C 管线':>12}{'差值':>10}")
for k in KS:
    o = off[k]/P*100; l = liv[k]/P*100
    print(f"{k:<4}{o:>11.1f}%{l:>11.1f}%{(l-o):>+9.1f}%")
if off_med and liv_med:
    off_med.sort(); liv_med.sort()
    print(f"中位最佳排名: 纯余弦={off_med[len(off_med)//2]}  A+C={liv_med[len(liv_med)//2]}")
con.close()
print("DONE")
