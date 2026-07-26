#!/usr/bin/env python3
"""Phase 0.3: 从干净 entity_mentions 整表重建 same_entity 边（去空名实体污染）。

- 快照由调用方负责（或本脚本 --no-snapshot 跳过）。
- 删除全部 relation_type='same_entity'，再从非空格实体重算共现边。
- 同 namespace 内共享 >=2 个真实实体（idf 降权）的记忆对 -> 边。
- 幂等：先 DELETE 同 type 再插；出边 cap 防爆炸。

用法:
  python rebuild_same_entity.py --db <path> [--cap 20] [--min-weight 0.1] [--min-shared 2]
"""
import argparse
import sqlite3
import math
import os
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--cap", type=int, default=20)
    ap.add_argument("--min-weight", type=float, default=0.1)
    ap.add_argument("--min-shared", type=int, default=2)
    ap.add_argument("--no-snapshot", action="store_true")
    args = ap.parse_args()

    if not args.no_snapshot:
        snap = args.db + ".same_entity_rebuild_bak"
        if os.path.exists(snap):
            print(f"[skip] snapshot exists: {snap}")
        else:
            try:
                import shutil
                shutil.copy2(args.db, snap)
                print(f"[ok] snapshot -> {snap}")
            except Exception as e:
                print(f"[fatal] snapshot failed: {e}", file=sys.stderr)
                sys.exit(2)

    c = sqlite3.connect(args.db)
    c.execute("PRAGMA foreign_keys=OFF")

    # 1) 清旧 same_entity（来源已被污染）
    old = c.execute("SELECT COUNT(*) FROM memory_relations WHERE relation_type='same_entity'").fetchone()[0]
    c.execute("DELETE FROM memory_relations WHERE relation_type='same_entity'")
    print(f"[ok] deleted old same_entity: {old}")

    # 2) 实体频度（idf 分母）：仅非空格实体
    ent_freq = dict(
        c.execute(
            "SELECT e.id, COUNT(*) FROM entity_mentions em "
            "JOIN entities e ON em.entity_id=e.id WHERE e.name<>'' GROUP BY e.id"
        )
    )
    if not ent_freq:
        print("[warn] no non-empty entities, nothing to rebuild")
        c.commit()
        c.close()
        return

    # 3) 每实体记忆数（用于出边 cap）
    mem_deg = dict(
        c.execute(
            "SELECT memory_id, COUNT(*) FROM entity_mentions em "
            "JOIN entities e ON em.entity_id=e.id WHERE e.name<>'' GROUP BY memory_id"
        )
    )

    # 4) 共现对：同 ns 内共享 >= min_shared 个真实实体的记忆对
    pairs = c.execute(
        """
        SELECT m1.memory_id, m2.memory_id, m1.namespace, COUNT(*) AS shared
        FROM entity_mentions m1
        JOIN entity_mentions m2
          ON m1.entity_id = m2.entity_id AND m1.memory_id < m2.memory_id
        JOIN entities e ON m1.entity_id = e.id
        WHERE e.name <> ''
        GROUP BY m1.memory_id, m2.memory_id, m1.namespace
        HAVING shared >= ?
        """,
        (args.min_shared,),
    ).fetchall()

    # 5) 聚合为无向边，weight = shared / (1 + log(平均实体频度))
    edge_w = {}  # (a,b,ns) -> (shared, sum_logfreq)
    for a, b, ns, shared in pairs:
        key = (a, b, ns)
        eid = c.execute(
            "SELECT entity_id FROM entity_mentions WHERE memory_id=? AND memory_id IN "
            "(SELECT memory_id FROM entity_mentions WHERE memory_id=?) LIMIT 1",
            (a, b),
        )
        # 用 shared 与两记忆各自实体频度估算 idf 权重
        fa = mem_deg.get(a, 1)
        fb = mem_deg.get(b, 1)
        idf = 1.0 / (1.0 + math.log(max(fa, fb)))
        w = shared * idf
        if key not in edge_w or w > edge_w[key][0]:
            edge_w[key] = (w, ns)

    # 6) 出边 cap + 阈值过滤 + 插入
    out_deg = {}
    ins_rows = []
    for (a, b, ns), (w, _ns) in edge_w.items():
        if w < args.min_weight:
            continue
        if out_deg.get(a, 0) >= args.cap or out_deg.get(b, 0) >= args.cap:
            continue
        ins_rows.append((ns, a, b, "same_entity", round(w, 4)))
        out_deg[a] = out_deg.get(a, 0) + 1
        out_deg[b] = out_deg.get(b, 0) + 1

    c.executemany(
        "INSERT INTO memory_relations(namespace, source_id, target_id, relation_type, weight) "
        "VALUES(?,?,?,?,?)",
        ins_rows,
    )
    c.commit()
    new_n = c.execute("SELECT COUNT(*) FROM memory_relations WHERE relation_type='same_entity'").fetchone()[0]
    print(f"[ok] inserted same_entity: {new_n} (from {len(pairs)} raw pairs, cap={args.cap}, min_w={args.min_weight})")
    c.close()


if __name__ == "__main__":
    main()
