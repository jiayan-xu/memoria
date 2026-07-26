"""轻量语义查询基准 — 验证 Phase 1b semantic_related 边的真实价值（review 第 6 条）。

与 eval_recall_corrected.py 的区别：
- 后者金标准 = same_entity / updates 可达强邻居（实体/时序图），语义边不在金标准内，故 1b 中性。
- 本脚本金标准 = **semantic_related 可达邻居**（仅嵌入相似图），专门测语义边是否被管线用上。

归因逻辑：
- 金标准 B 是 A 的「纯语义」邻居（非 same_entity/updates），故若完整管线(向量+图扩展含 semantic 边)
  能在 top-k 召回 B，而纯向量检索漏掉 B，则该提升**特定来自 1b 的 semantic 图遍历**。

度量：
- FULL：memory_recall(query=content(A)) 的 top-k 命中金标准比例。
- VECTOR-ONLY：离线纯余弦(content(A) 向量) top-k 命中比例（不依赖任何图扩展）。
- lift = FULL - VECTOR-ONLY（即 1b 语义图遍历贡献）。

P0：禁硬编码用户名绝对路径；DB/ENV 走 env 注入，回退相对路径。
"""
import sqlite3, json, urllib.request, time, array, os
import numpy as np

DB = os.environ.get("MEMORIA_DB_PATH", "data/memoria.db")
ENV_PATH = os.environ.get("MEMORIA_ENV_PATH", ".env")
MCP_URL = "http://127.0.0.1:9003/mcp"
NS = "agent/xujiayan"
KS = [1, 3, 5, 10]
SAMPLE_N = int(os.environ.get("SEMANTIC_SAMPLE_N", "40"))
SIM_MIN = float(os.environ.get("SEMANTIC_GOLD_SIM_MIN", "0.60"))  # 与建边阈值一致


def load_env(p):
    d = {}
    if not os.path.exists(p):
        return d
    for line in open(p, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        d[k.strip()] = v.strip().strip('"').strip("'")
    return d


ADMIN_KEY = load_env(ENV_PATH).get("MEMORIA_ADMIN_KEY", "")
if not ADMIN_KEY:
    raise SystemExit("MEMORIA_ADMIN_KEY not found in %s" % ENV_PATH)


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

# --- 向量矩阵 ---
cur.execute("SELECT v.id, v.vector FROM memory_vectors v JOIN memories m ON v.id=m.id WHERE m.namespace=?", (NS,))
ids, vecs = [], []
for mid, blob in cur.fetchall():
    a = array.array("f"); a.frombytes(blob); ids.append(mid); vecs.append(list(a))
mat = np.array(vecs, dtype=np.float32)
norms = np.linalg.norm(mat, axis=1, keepdims=True); norms[norms == 0] = 1.0
mat = mat / norms
idx_of = {mid: i for i, mid in enumerate(ids)}

# --- 可达性 ---
cur.execute("SELECT id, superseded_by, valid_from, valid_to FROM memories WHERE namespace=?", (NS,))
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


# --- 语义金标准：semantic_related 双向可达邻居 ---
gold = {}  # src -> set(tgt)
cur.execute("""SELECT source_id, target_id FROM memory_relations
               WHERE relation_type='semantic_related' AND namespace=?""", (NS,))
for s, t in cur.fetchall():
    if reachable(s) and reachable(t) and s != t:
        gold.setdefault(s, set()).add(t)

# --- 内容表 ---
cur.execute("SELECT id, content FROM memories WHERE namespace=?", (NS,))
content = {mid: c for mid, c in cur.fetchall()}

# --- 候选探针：有语义邻居、邻居相似度落在 [SIM_MIN, 0.78]（避免 B 太近导致纯向量也必中）---
probes = []
for s, tgts in gold.items():
    if s not in idx_of:
        continue
    sv = mat[idx_of[s]]
    acceptable = [t for t in tgts if t in idx_of and (SIM_MIN <= float(np.dot(sv, mat[idx_of[t]])) <= 0.78)]
    if acceptable:
        probes.append((s, set(acceptable)))
# 去重并按邻居数抽样
probes = probes[:SAMPLE_N]
if not probes:
    raise SystemExit("no semantic probes found (check edges / NS)")

print(f"semantic probes: {len(probes)} (NS={NS}, SIM_MIN={SIM_MIN})")


def recall_at(got, gold_set, k):
    got_k = set(got[:k])
    hit = len(got_k & gold_set)
    return hit / len(gold_set) if gold_set else 0.0


full_sum = {k: 0.0 for k in KS}
vec_sum = {k: 0.0 for k in KS}
per_probe = []

for i, (s, gset) in enumerate(probes):
    q = content.get(s, "")
    if not q:
        continue
    # FULL 管线
    got = recall_ids(q, max_results=15)
    # VECTOR-ONLY 基线（离线纯余弦）
    sv = mat[idx_of[s]]
    cos = mat @ sv
    order = np.argsort(-cos)
    vec_top = [ids[j] for j in order[:15]]

    row = {"src": s, "gold_n": len(gset)}
    for k in KS:
        fr = recall_at(got, gset, k)
        vr = recall_at(vec_top, gset, k)
        full_sum[k] += fr
        vec_sum[k] += vr
        row[f"full@{k}"] = round(fr, 2)
        row[f"vec@{k}"] = round(vr, 2)
    per_probe.append(row)
    if (i + 1) % 10 == 0:
        print(f"  ...{i+1}/{len(probes)}")

n = len(per_probe)
print(f"\n=== 语义召回基准（金标准=semantic_related 可达邻居, n={n}）===")
print(f"{'k':>4} | {'FULL(向量+图)':>14} | {'VECTOR-ONLY':>12} | {'1b lift':>8}")
for k in KS:
    fv = full_sum[k] / n
    vv = vec_sum[k] / n
    print(f"{k:>4} | {fv*100:>12.1f}% | {vv*100:>10.1f}% | {(fv-vv)*100:>+6.1f}pp")

print("\n--- 样本（前 8，full/vec @10）---")
for r in per_probe[:8]:
    print(f"  {r['src'][:12]} gold={r['gold_n']} full@10={r.get('full@10')} vec@10={r.get('vec@10')}")
print("DONE")
