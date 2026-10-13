#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""无向量记忆批量补嵌（P0-2，2026-10-13）。

背景
----
10-13 检索故障诊断发现生产库 9749 条记忆中 995 条（10.2%）无向量——
① 文档 ingestion 路径产物（602 条）：`document.rs` 调 remember_with_dedup 时
   hnsw/query_cache 恒传 None，candidate_vector 结构上恒空（P0-1 已修代码）；
② 2026-09~10 部分 MCP 写入（352 条）：与提权 rogue 实例活跃期重叠，
   嵌入通道在其进程内未生效（深因随实例火化，无法复现）。
这些记忆仅能被 FTS 捞到，语义召回（HNSW）完全不可见。

与 rebuild_vectors.py 的区别
----------------------------
- `rebuild_vectors.py`：对指定 ns 的**活跃记忆全量重嵌**（修"污染向量"——
  不同内容被存成近似向量导致近义去重误命中）。
- 本脚本：对**缺失向量**的记忆补嵌（修"覆盖缺口"），跨全部 ns、含文档块，
  默认只处理 `memory_vectors` 中不存在的 id。

用法
----
    python scripts/backfill_missing_vectors.py            # 补全部缺口
    python scripts/backfill_missing_vectors.py --dry-run  # 只算不写
    python scripts/backfill_missing_vectors.py --limit 50 # 小批试跑
    python scripts/backfill_missing_vectors.py --ns agent/xujiayan

安全
----
- 只读扫描；嵌入结果经"退化（零/NaN）拒绝 + 维度校验"后才收集；
- 写回前抽样一致性安全闸（新向量 vs 单条实时嵌入余弦 >= 0.9，超 20% 偏
  差则中止，数据零改动——沿用 rebuild_vectors.py 的 2026-08-06 教训）；
- 最终单事务写回（INSERT ... ON CONFLICT DO UPDATE），可整体回滚。

重要
----
服务运行中即可写（WAL + busy_timeout 60s），但新向量**需重启服务**才会进入
内存 HNSW 索引——`memory_vectors` 表是权威持久源，启动时 rebuild（P1-3）。
因此标准流程：跑本脚本 → restart.ps1 重启 → /health 核对索引条数。
"""
import argparse
import json
import os
import sqlite3
import sys
import time
import urllib.request

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
DB = os.environ.get(
    "MEMORIA_DB_PATH",
    os.path.join(HERE, "..", "..", "memoria", "data", "memoria.db"),
)
# 2026-10-13：默认 8778（本地 bge-m3-int8 实际端口）。exe 与旧脚本默认的
# 8777 是死端口（10-02/10-13 两次事故的根），这里必须显式钉住。
EMBED_URL = os.environ.get("MEMORIA_EMBEDDING_URL", "http://127.0.0.1:8778/embed")
DIM = 1024


def _is_degenerate(v):
    """零向量 / NaN 判定（嵌入服务高负载下会返回缺项并兜底填零）。"""
    if not v:
        return True
    try:
        return sum(x * x for x in v) < 1e-9
    except TypeError:
        return True


def embed_batch(texts, retries=4):
    """调嵌入服务嵌一批；含退化向量则整体重试，绝不把零向量交回上层。

    沿用 rebuild_vectors.py 2026-08-22 修复：批量请求可能静默返回缺项并被
    兜底填零，污染 memory_vectors。这里收响应后立即校验，退化则整批重试。
    """
    last_err = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(
                EMBED_URL,
                data=json.dumps({"texts": texts}).encode(),
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            d = json.loads(urllib.request.urlopen(req, timeout=180).read())
            vecs = d.get("embeddings") or d.get("vectors")
            if vecs is None or len(vecs) != len(texts):
                last_err = f"返回数量不符 {len(vecs) if vecs else 0}/{len(texts)}"
            elif any(_is_degenerate(v) for v in vecs):
                last_err = "含零/退化向量（高负载静默退化），重试"
            else:
                return vecs
        except Exception as e:  # noqa: BLE001 — 网络/解析异常一律重试
            last_err = str(e)
        time.sleep(2 * (attempt + 1))
    print(f"  [FAIL] batch {len(texts)} 重试 {retries} 次失败: {last_err}")
    return None


def main():
    ap = argparse.ArgumentParser()
    # 2026-08-22 教训：batch 16 会触发批量请求缺项→零向量污染；8 实测干净。
    ap.add_argument("--batch", type=int, default=8)
    ap.add_argument("--dry-run", action="store_true", help="只算不写")
    ap.add_argument("--limit", type=int, default=0, help="只处理前 N 条（调试）")
    ap.add_argument("--ns", default="", help="只处理指定 namespace（默认全部）")
    args = ap.parse_args()

    con = sqlite3.connect(DB, timeout=60)
    con.execute("PRAGMA busy_timeout=60000")
    cur = con.cursor()
    total = cur.execute("SELECT COUNT(*) FROM memories").fetchone()[0]
    have = cur.execute("SELECT COUNT(*) FROM memory_vectors").fetchone()[0]

    sql = """SELECT m.id, m.namespace, m.content
               FROM memories m
               LEFT JOIN memory_vectors v ON v.id = m.id
              WHERE v.id IS NULL
                AND length(coalesce(m.content, '')) > 0"""
    params = []
    if args.ns:
        sql += " AND m.namespace = ?"
        params.append(args.ns)
    sql += " ORDER BY m.created_at DESC"
    rows = cur.execute(sql, params).fetchall()
    if args.limit:
        rows = rows[: args.limit]
    con.close()

    gap = total - have
    print(f"记忆总数 {total} | 已有向量 {have} | 覆盖缺口 {gap} "
          f"({gap / total * 100:.1f}%)")
    print(f"本次待补嵌: {len(rows)}")
    if not rows:
        print("无缺口，退出")
        return

    new_vectors = []  # (id, namespace, vec)
    t0 = time.time()
    for i in range(0, len(rows), args.batch):
        chunk = rows[i:i + args.batch]
        # 与写入路径一致：嵌全文（P0-1 后 MCP 写入侧与文档 ingestion 均嵌
        # 全文）。历史 rebuild_vectors.py 的 [:1500] 截断会丢文档块尾部语义，
        # 且造成"写入侧全文 / 补嵌侧截断"的向量空间不一致。
        texts = [c for _, _, c in chunk]
        vecs = embed_batch(texts)
        if vecs is None:
            print(f"!! 批次 {i // args.batch} 失败，中止（无写入）")
            sys.exit(1)
        for (mid, ns, _), v in zip(chunk, vecs):
            if len(v) != DIM:
                print(f"!! 维度异常 {len(v)}（应为 {DIM}）@ {mid}，中止")
                sys.exit(1)
            new_vectors.append((mid, ns, v))
        done = i + len(chunk)
        if done % (args.batch * 10) == 0 or done == len(rows):
            el = time.time() - t0
            eta = el / done * (len(rows) - done)
            print(f"  ...{done}/{len(rows)} ({el:.0f}s, 剩余约 {eta:.0f}s)")

    print(f"\n嵌入完成: {len(new_vectors)} 条, 耗时 {time.time() - t0:.0f}s")
    if args.dry_run:
        print("[dry-run] 未写库")
        return

    # 安全闸（2026-08-06 教训）：写回前抽样验证"批嵌向量 vs 单条实时嵌入"
    # 一致性，偏差过多说明嵌入服务输出不可信，整体中止、数据零改动。
    import random
    random.seed(42)
    probe = random.sample(new_vectors, min(20, len(new_vectors)))
    bad = 0
    for mid, _ns, v in probe:
        content = next((c for m, _n, c in rows if m == mid), "")
        if not content:
            continue
        vecs = embed_batch([content])
        if vecs is None:
            bad += 1
            continue
        lv = np.array(vecs[0], dtype=np.float32)
        sv = np.array(v, dtype=np.float32)
        denom = (np.linalg.norm(sv) * np.linalg.norm(lv) + 1e-9)
        if float(sv @ lv / denom) < 0.9:
            bad += 1
    if bad > max(1, len(probe) * 0.2):
        print(f"❌ 安全闸拦截: 抽样 {len(probe)} 条中 {bad} 条一致性 <0.9 —— "
              f"嵌入服务输出不可信，中止写回（数据未改动）")
        sys.exit(3)
    print(f"✅ 安全闸通过: 抽样 {len(probe)} 条, {len(probe) - bad} 条一致性 >=0.9")

    # 写回（单事务）
    con = sqlite3.connect(DB, timeout=120)
    con.execute("PRAGMA busy_timeout=120000")
    try:
        con.execute("BEGIN")
        cur = con.cursor()
        now = time.strftime("%Y-%m-%dT%H:%M:%S")
        for mid, ns, v in new_vectors:
            blob = np.array(v, dtype=np.float32).tobytes()
            cur.execute(
                "INSERT INTO memory_vectors (id, namespace, vector, updated_at) "
                "VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET "
                "vector=excluded.vector, namespace=excluded.namespace, "
                "updated_at=excluded.updated_at",
                (mid, ns, blob, now),
            )
        con.commit()
        print(f"✅ 写回完成: {len(new_vectors)} 条")
    except Exception as e:  # noqa: BLE001
        con.rollback()
        print(f"❌ 写回失败已回滚: {e}")
        sys.exit(1)
    finally:
        con.close()

    # 收尾核对
    con = sqlite3.connect(DB, timeout=60)
    con.execute("PRAGMA busy_timeout=60000")
    t = con.execute("SELECT COUNT(*) FROM memories").fetchone()[0]
    h = con.execute("SELECT COUNT(*) FROM memory_vectors").fetchone()[0]
    con.close()
    print(f"补嵌后覆盖: {h}/{t} = {h / t * 100:.1f}%（缺口 {t - h}）")
    print("⚠️  新向量需重启服务才进内存 HNSW：请执行 restart.ps1 后核对 /health")


if __name__ == "__main__":
    main()
