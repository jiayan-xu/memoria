#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""图召回定论分析器（2026-07-26）。

三件事：
1) 确认 graph_on.json 与 graph_off.json 的 top-10 命中集合是否一致（Δ@10 的明细级验证）。
2) 桥接可达性：对「完全不在池」的 gold，以本地余弦算每条 query 的语义 top-10 为种子，
   在 memory_relations 上做 2 跳 BFS，判断该 gold 是否 *真的* 处于相关种子的 2 跳邻域。
   - 若不可达 → 图扩展结构上根本无法召回这些 gold（关系没编码相关连接）。
   - 若可达但没进 top-10 → 是重排/主通道保底把它们挡在门外（可调但 ROI 低）。
3) 抽样 probe：对若干 query 调 memory_recall(ON)，统计返回 100 池中 graph_expand 项的
   数量与最佳位置，确认图项是否进入候选池、被压在哪。
"""
import sqlite3, json, urllib.request, os, array, math, time, sys
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
DB = os.path.join(ROOT, "..", "memoria", "data", "memoria.db")
NS = "agent/xujiayan"
GOLD = os.path.join(HERE, "hyde_queries.json")
EMB_URL = "http://127.0.0.1:8777/embed"
MCP_URL = "http://127.0.0.1:9003/mcp"

def load_env(p):
    d = {}
    for line in open(p, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1); d[k.strip()] = v.strip().strip('"').strip("'")
    return d

ENV = load_env(os.path.join(ROOT, "..", "memoria", ".env"))
ADMIN = ENV.get("MEMORIA_ADMIN_KEY", "")

def embed(text):
    req = urllib.request.Request(EMB_URL, data=json.dumps({"texts": [text]}).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    return np.array(json.loads(urllib.request.urlopen(req, timeout=60).read())["embeddings"][0], dtype=np.float32)

def recall_pool(query):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_recall",
                          "arguments": {"query": query, "namespace": NS, "max_results": 100}}}
    req = urllib.request.Request(MCP_URL, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json",
                                          "X-Agent-Id": "admin", "X-Agent-Key": ADMIN}, method="POST")
    return json.loads(urllib.request.urlopen(req, timeout=90).read())["result"]["content"][0]["text"]

def main():
    on = {d["id"]: d for d in json.load(open(os.path.join(HERE, "graph_on.json"), encoding="utf-8"))}
    off = {d["id"]: d for d in json.load(open(os.path.join(HERE, "graph_off.json"), encoding="utf-8"))}
    # 1) top-10 集合一致性
    on_top10 = {i for i, d in on.items() if d["in_pool"] and d["pool_pos"] and d["pool_pos"] <= 10}
    off_top10 = {i for i, d in off.items() if d["in_pool"] and d["pool_pos"] and d["pool_pos"] <= 10}
    print(f"[1] top-10 命中集合: ON={len(on_top10)} OFF={len(off_top10)} 交集={len(on_top10 & off_top10)} 对称差={on_top10 ^ off_top10}")
    print(f"    ON 独有: {on_top10 - off_top10}")
    print(f"    OFF 独有: {off_top10 - on_top10}")

    # 载入向量 + 图
    con = sqlite3.connect(DB, timeout=30); con.execute("PRAGMA busy_timeout=30000")
    cur = con.cursor()
    cur.execute("""SELECT v.id, v.vector FROM memory_vectors v JOIN memories m ON v.id=m.id WHERE m.namespace=?""", (NS,))
    ids, vecs = [], []
    for mid, blob in cur.fetchall():
        a = array.array("f"); a.frombytes(blob); ids.append(mid); vecs.append(list(a))
    M = np.array(vecs, dtype=np.float32); norms = np.linalg.norm(M, axis=1, keepdims=True); norms[norms==0]=1.0; M = M/norms
    id2row = {mid: i for i, mid in enumerate(ids)}

    # 关系邻接表（双向，ns 限定）
    adj = {}
    for s, t, w, rt in cur.execute(
        "SELECT source_id, target_id, weight, relation_type FROM memory_relations WHERE namespace=? AND weight>0", (NS,)):
        adj.setdefault(s, []).append((t, w, rt))
        adj.setdefault(t, []).append((s, w, rt))
    con.close()

    bench = json.load(open(GOLD, encoding="utf-8"))["items"]
    not_in_pool = [it["id"] for it in bench if it["id"] in on and not on[it["id"]]["in_pool"]]
    print(f"\n[2] 完全不在池的 gold: {len(not_in_pool)} 条")

    reachable = []      # 可达（结构上能召回）
    unreachable = []    # 不可达
    t0 = time.time()
    for idx, it in enumerate(bench):
        mid, q = it["id"], it["query"]
        if mid not in not_in_pool:
            continue
        gi = id2row[mid]
        vq = embed(q); nrm = np.linalg.norm(vq)
        if nrm == 0: continue
        vq = vq / nrm
        cos = M @ vq
        # 语义 top-10 种子
        order = np.argsort(-cos)[:10]
        seeds = [ids[i] for i in order]
        # 2 跳 BFS
        visited = set(seeds)
        frontier = list(seeds)
        hit_hop = None
        for hop in (1, 2):
            nxt = []
            for fid in frontier:
                for (nb, w, rt) in adj.get(fid, []):
                    if nb == mid:
                        hit_hop = hop
                        break
                    if nb not in visited:
                        visited.add(nb); nxt.append(nb)
                if hit_hop: break
            if hit_hop: break
            frontier = nxt
        if hit_hop:
            reachable.append((mid, hit_hop, round(float(cos[gi]), 3)))
        else:
            unreachable.append((mid, round(float(cos[gi]), 3)))
        if (idx + 1) % 10 == 0:
            print(f"  ...{idx+1} ({time.time()-t0:.0f}s)")
    print(f"    可达（相关种子 2 跳内）: {len(reachable)} 条 -> {reachable}")
    print(f"    不可达（图结构上接不到）: {len(unreachable)} 条 -> {unreachable}")

    # 3) 抽样 probe 池内 graph_expand 项
    print(f"\n[3] 抽样 probe（graph ON 实例）：统计返回 100 池内 graph_expand 项的位置")
    sample = bench[:6]
    for it in sample:
        q = it["query"]
        data = json.loads(recall_pool(q))
        pool = data.get("results", [])
        gitems = [(i+1, x.get("source","")) for i, x in enumerate(pool) if "graph_expand" in (x.get("source") or "")]
        print(f"  q='{q[:30]}' 池大小={len(pool)} graph项={len(gitems)} 最佳位置={min([p for p,_ in gitems], default=None)}")

if __name__ == "__main__":
    main()
