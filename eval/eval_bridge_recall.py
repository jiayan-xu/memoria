"""2-hop 桥接召回基准 — 公平专项验证 Phase 1b（semantic_related 边）的间接价值。

与 eval_semantic_recall.py（1-hop 直连）的区别：
- 上轮语义基准用「查询=记忆 A 原文」，此时 A 的语义邻居 B 已被 S2 向量通道直接召回
  （cos(A,B)>=0.6），1b 图边完全冗余 → 测不出 1b 价值，且暴露了 two_stage_rerank 稀释语义项。
- 本基准测真正需要图边的场景：金标准 = 通过 B 桥接的 **2-hop 非向量直连** 记忆 C。
    A --semantic_related(>=0.6)--> B --semantic_related(>=0.6)--> C
    且 cos(A, C) < BRIDGE_MAX(0.5)：A 与 C 不直接语义相似 → 纯向量检索召回不了 C，
    只有 graph_expand 从 B 走 1-hop 才能把 C 拉进候选池。这正是 1b 边的理论价值点。

度量：
- FULL：memory_recall(query=A 原文) top-k 命中 C 的比例（含 graph_expand 2-hop）。
- VECTOR-ONLY：离线纯余弦(A 向量) top-k 命中 C（cos(A,C)<0.5，应极低）。
- lift = FULL - VECTOR-ONLY（即 1b 桥接贡献；若仍≈0，说明 1b 被重排稀释而非无效）。

P0：禁硬编码用户名绝对路径；DB/ENV 走 env 注入。
"""
import sqlite3, json, urllib.request, time, array, os
import numpy as np

DB = os.environ.get("MEMORIA_DB_PATH", "data/memoria.db")
ENV_PATH = os.environ.get("MEMORIA_ENV_PATH", ".env")
MCP_URL = "http://127.0.0.1:9003/mcp"
NS = "agent/xujiayan"
KS = [1, 3, 5, 10, 15]
SAMPLE_N = int(os.environ.get("BRIDGE_SAMPLE_N", "40"))
SIM_AB_LO = float(os.environ.get("BRIDGE_SIM_AB_LO", "0.60"))   # A-B 建边阈值
SIM_AB_HI = float(os.environ.get("BRIDGE_SIM_AB_HI", "0.78"))   # 避免 B 太近
SIM_BC_LO = float(os.environ.get("BRIDGE_SIM_BC_LO", "0.60"))   # B-C 建边阈值
BRIDGE_MAX = float(os.environ.get("BRIDGE_MAX", "0.50"))        # A-C 非直连上限


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


def recall_ids(query, max_results=100):
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


# --- semantic_related 邻接（双向） ---
adj = {}
cur.execute("""SELECT source_id, target_id FROM memory_relations
               WHERE relation_type='semantic_related' AND namespace=?""", (NS,))
for s, t in cur.fetchall():
    if reachable(s) and reachable(t) and s != t:
        adj.setdefault(s, set()).add(t)
        adj.setdefault(t, set()).add(s)

# --- 内容表 ---
cur.execute("SELECT id, content FROM memories WHERE namespace=?", (NS,))
content = {mid: c for mid, c in cur.fetchall()}

# --- 桥接金标准：A -(sem)>=SIM_AB- B -(sem)>=SIM_BC- C，且 cos(A,C)<BRIDGE_MAX，C 非直连 ---
probes = []  # (A, set(C))
for A, nbrs in adj.items():
    if A not in idx_of or not content.get(A):
        continue
    av = mat[idx_of[A]]
    for B in nbrs:
        if B not in idx_of:
            continue
        bv = mat[idx_of[B]]
        if not (SIM_AB_LO <= float(np.dot(av, bv)) <= SIM_AB_HI):
            continue
        for C in adj.get(B, ()):
            if C == A or C == B or C not in idx_of:
                continue
            cv = mat[idx_of[C]]
            sim_ac = float(np.dot(av, cv))
            if sim_ac >= BRIDGE_MAX:
                continue  # A-C 直连，不算桥接
            sim_bc = float(np.dot(bv, cv))
            if sim_bc < SIM_BC_LO:
                continue
            if not reachable(C):
                continue
            probes.append((A, C, sim_ac, sim_bc))

# 合并同 A 的 C 集合，按 C 数抽样
from collections import defaultdict
byA = defaultdict(list)
for A, C, sac, sbc in probes:
    byA[A].append((C, sac, sbc))
byA = dict(list(byA.items())[:SAMPLE_N])
if not byA:
    raise SystemExit("no bridge probes found (check edges / NS / thresholds)")

print(f"bridge probes: {len(byA)} A-nodes, {sum(len(v) for v in byA.values())} (A,C) pairs "
      f"(NS={NS}, SIM_AB=[{SIM_AB_LO},{SIM_AB_HI}], SIM_BC>={SIM_BC_LO}, A-C<{BRIDGE_MAX})")

flat = [(A, C, sac, sbc) for A, lst in byA.items() for (C, sac, sbc) in lst]


def recall_at(got, gold_set, k):
    got_k = set(got[:k])
    hit = len(got_k & gold_set)
    return hit / len(gold_set) if gold_set else 0.0


full_sum = {k: 0.0 for k in KS}
vec_sum = {k: 0.0 for k in KS}
full_pool_sum = 0.0  # C 是否进候选池前 100（隔离「拉进」vs「重排压低」）
per = []

for i, (A, C, sac, sbc) in enumerate(flat):
    q = content[A]
    got = recall_ids(q, max_results=100)  # 取大候选池，隔离「是否被图拉进」vs「被重排压低」
    av = mat[idx_of[A]]
    cos = mat @ av
    order = np.argsort(-cos)
    vec_top = [ids[j] for j in order[:15]]

    row = {"A": A[:12], "C": C[:12], "simAC": round(sac, 2), "simBC": round(sbc, 2)}
    full_pool_sum += recall_at(got, {C}, 100)
    for k in KS:
        fr = recall_at(got, {C}, k)
        vr = recall_at(vec_top, {C}, k)
        full_sum[k] += fr
        vec_sum[k] += vr
        row[f"f@{k}"] = round(fr, 2)
        row[f"v@{k}"] = round(vr, 2)
    per.append(row)
    if (i + 1) % 10 == 0:
        print(f"  ...{i+1}/{len(flat)}")

n = len(flat)
print(f"\n=== 2-hop 桥接召回基准（金标准=非直连语义桥接记忆 C, n={n} 对）===")
print(f"{'k':>4} | {'FULL(含图)':>11} | {'VECTOR-ONLY':>12} | {'1b lift':>8}")
for k in KS:
    fv = full_sum[k] / n
    vv = vec_sum[k] / n
    print(f"{k:>4} | {fv*100:>9.1f}% | {vv*100:>10.1f}% | {(fv-vv)*100:>+6.1f}pp")
print(f"--- 候选池命中（FULL 取 max_results=100，C 是否进前 100）：{full_pool_sum/n*100:.1f}% ---")
print("    若 pool>>top15：说明 1b 把 C 拉进了候选池，但 two_stage_rerank 把它压出 top-15（重排稀释根因）。")
print("    若 pool≈0：说明 graph_expand 根本没拉进 C（种子/配额问题），与重排无关。")

print("\n--- 样本（前 10，f/v @10, simAC/simBC）---")
for r in per[:10]:
    print(f"  A={r['A']} C={r['C']} simAC={r['simAC']} simBC={r['simBC']} f@10={r.get('f@10')} v@10={r.get('v@10')}")
print("DONE")
