#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""agent/xujiayan 活跃记忆全量重嵌（2026-08-06 向量污染修复）。

背景：排查发现 memory_vectors 存在系统性坏向量——内容不同的记忆被存成几乎相同
的向量（>0.98 高相似涉及活跃记忆 25.5%），导致近义去重误命中、语义召回失真。
本脚本对 agent/xujiayan 全部活跃记忆（superseded_by IS NULL 且未过期）用嵌入服务
重新计算向量，全部成功后单事务写回 memory_vectors（避免半新半旧不一致）。

用法：python rebuild_vectors.py [--batch 16] [--dry-run]
安全：只读扫描 + 最终单事务写回；可整体回滚（备份在 backups/）。
"""
import sqlite3, json, urllib.request, time, sys, os, argparse
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
DB = os.environ.get("MEMORIA_DB_PATH",
                    os.path.join(HERE, "..", "memoria", "data", "memoria.db"))
EMBED_URL = os.environ.get("MEMORIA_EMBEDDING_URL", "http://127.0.0.1:8777/embed")
NS = os.environ.get("MEMORIA_GUARD_NS", "agent/xujiayan")


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


def embed_batch(texts, retries=3):
    """调用嵌入服务，单批；失败重试。返回向量列表或 None。"""
    last_err = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(
                EMBED_URL, data=json.dumps({"texts": texts}).encode(),
                headers={"Content-Type": "application/json"}, method="POST")
            d = json.loads(urllib.request.urlopen(req, timeout=180).read())
            vecs = d.get("vectors") or d.get("embeddings")
            if vecs is None or len(vecs) != len(texts):
                last_err = f"返回数量不符 {len(vecs) if vecs else 0}/{len(texts)}"
            else:
                return vecs
        except Exception as e:
            last_err = str(e)
        time.sleep(2 * (attempt + 1))
    print(f"  [FAIL] batch {len(texts)} 重试3次失败: {last_err}")
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--dry-run", action="store_true", help="只算不写")
    ap.add_argument("--limit", type=int, default=0, help="只处理前 N 条（调试）")
    args = ap.parse_args()

    con = sqlite3.connect(DB, timeout=60)
    con.execute("PRAGMA busy_timeout=60000")
    cur = con.cursor()
    rows = cur.execute(
        f"""SELECT id, content FROM memories
            WHERE namespace=? AND length(coalesce(content,''))>0
              AND superseded_by IS NULL
              AND (valid_to IS NULL OR valid_to='' OR valid_to>datetime('now'))
            ORDER BY RANDOM()""", (NS,)).fetchall()
    if args.limit:
        rows = rows[:args.limit]
    con.close()
    print(f"待重嵌活跃记忆: {len(rows)}")

    new_vectors = []   # (id, bytes)
    t0 = time.time()
    for i in range(0, len(rows), args.batch):
        chunk = rows[i:i + args.batch]
        texts = [c[:1500] for _, c in chunk]
        vecs = embed_batch(texts)
        if vecs is None:
            print(f"!! 批次 {i//args.batch} 失败，中止（无写入）")
            sys.exit(1)
        for (mid, _), v in zip(chunk, vecs):
            if len(v) != 1024:
                print(f"!! 维度异常 {len(v)} @ {mid}，中止")
                sys.exit(1)
            new_vectors.append((mid, v))
        if (i // args.batch) % 20 == 0:
            el = time.time() - t0
            print(f"  ...{i+len(chunk)}/{len(rows)} ({el:.0f}s, 预计 {el/(i+len(chunk))*len(rows):.0f}s)")

    print(f"\n嵌入完成: {len(new_vectors)} 条, 耗时 {time.time()-t0:.0f}s")
    if args.dry_run:
        print("[dry-run] 未写库")
        return

    # 写回（单事务）
    con = sqlite3.connect(DB, timeout=120)
    con.execute("PRAGMA busy_timeout=120000")
    try:
        con.execute("BEGIN")
        cur = con.cursor()
        n_upd = n_ins = 0
        for mid, v in new_vectors:
            blob = np.array(v, dtype=np.float32).tobytes()
            r = cur.execute("UPDATE memory_vectors SET vector=?, updated_at=? WHERE id=?",
                            (blob, time.strftime("%Y-%m-%dT%H:%M:%S"), mid))
            if r.rowcount == 0:
                cur.execute("INSERT INTO memory_vectors (id, namespace, vector, updated_at) VALUES (?,?,?,?)",
                            (mid, NS, blob, time.strftime("%Y-%m-%dT%H:%M:%S")))
                n_ins += 1
            else:
                n_upd += 1
        con.commit()
        print(f"✅ 写回完成: 更新 {n_upd}, 新增 {n_ins}")
    except Exception as e:
        con.rollback()
        print(f"❌ 写回失败已回滚: {e}")
        sys.exit(1)
    finally:
        con.close()


if __name__ == "__main__":
    main()
