#!/usr/bin/env python3
"""Phase 1b: 按 namespace 离线补 semantic_related 嵌入相似边（零 LLM）。

- 从 memory_vectors 取 1024d 向量，按 namespace 分块（互不跨 ns 连边）。
- 每块行归一化后 M = V @ V.T，每行取 top-k（排除自身），cosine > 阈值 建边。
- 插入 memory_relations(relation_type='semantic_related', weight=cosine)。
- 出边 cap <=8；幂等（先 DELETE 该 ns 的 semantic_related 再插）。
- 阈值/ k 默认 0.60 / 8（已知向量错存 87 例，偏高更稳）；可网格扫描调。

用法:
  python build_semantic_edges.py --db <path> [--threshold 0.60] [--k 8] [--cap 8]
"""
import argparse
import sqlite3
import math
import os
import sys
import numpy as np


def load_vectors(c, ns):
    rows = c.execute(
        "SELECT id, vector FROM memory_vectors WHERE namespace=?", (ns,)
    ).fetchall()
    ids = [r[0] for r in rows]
    vecs = []
    for r in rows:
        blob = r[1]
        # 向量以 float32 little-endian 存 BLOB
        arr = np.frombuffer(blob, dtype="<f4")
        vecs.append(arr)
    if not vecs:
        return ids, np.zeros((0, 0), dtype="<f4")
    M = np.stack(vecs).astype("<f4")
    return ids, M


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--threshold", type=float, default=0.60)
    ap.add_argument("--k", type=int, default=8)
    ap.add_argument("--cap", type=int, default=8)
    ap.add_argument("--no-snapshot", action="store_true")
    args = ap.parse_args()

    if not args.no_snapshot:
        snap = args.db + ".semantic_rebuild_bak"
        if not os.path.exists(snap):
            import shutil
            shutil.copy2(args.db, snap)
            print(f"[ok] snapshot -> {snap}")

    c = sqlite3.connect(args.db)
    c.execute("PRAGMA foreign_keys=OFF")

    nss = [r[0] for r in c.execute(
        "SELECT DISTINCT namespace FROM memory_vectors"
    ).fetchall()]
    print(f"[info] namespaces: {nss}")

    total_edges = 0
    for ns in nss:
        ids, M = load_vectors(c, ns)
        n = len(ids)
        if n < 2:
            print(f"[skip] ns={ns} only {n} vectors")
            continue
        # 行归一化
        norms = np.linalg.norm(M, axis=1, keepdims=True)
        norms[norms == 0] = 1.0
        Mn = M / norms
        Sim = Mn @ Mn.T  # (n,n) cosine
        np.fill_diagonal(Sim, -2.0)  # 排除自身
        # 每行 top-k
        kk = min(args.k, n - 1)
        # 取每行最大的 kk 个索引
        part = np.argpartition(-Sim, kk, axis=1)[:, :kk]
        edges = []
        out_deg = {}
        for i in range(n):
            for j in part[i]:
                j = int(j)
                if j >= n:
                    continue
                cos = float(Sim[i, j])
                if cos < args.threshold:
                    continue
                a, b = ids[i], ids[j]
                if a == b:
                    continue
                # 无向去重：保证 (min,max)
                key = (a, b) if a < b else (b, a)
                if out_deg.get(key[0], 0) >= args.cap or out_deg.get(key[1], 0) >= args.cap:
                    continue
                edges.append((ns, key[0], key[1], "semantic_related", round(cos, 4)))
                out_deg[key[0]] = out_deg.get(key[0], 0) + 1
                out_deg[key[1]] = out_deg.get(key[1], 0) + 1
        # 幂等：清空该 ns 的旧 semantic_related
        c.execute(
            "DELETE FROM memory_relations WHERE relation_type='semantic_related' AND namespace=?",
            (ns,),
        )
        c.executemany(
            "INSERT INTO memory_relations(namespace, source_id, target_id, relation_type, weight) "
            "VALUES(?,?,?,?,?)",
            edges,
        )
        total_edges += len(edges)
        print(f"[ok] ns={ns} vectors={n} semantic_edges={len(edges)} (th={args.threshold}, k={args.k})")

    c.commit()
    print(f"[done] total semantic_related inserted: {total_edges}")
    c.close()


if __name__ == "__main__":
    main()
