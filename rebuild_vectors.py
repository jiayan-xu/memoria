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


def _is_degenerate(v):
    """零向量 / 空向量判定（SiliconFlow 高负载下会返回缺项并兜底填零）。"""
    if not v:
        return True
    return sum(x * x for x in v) < 1e-9


def embed_batch(texts, retries=4):
    """调用嵌入服务，单批；失败/退化重试。返回向量列表或 None。

    关键修复（2026-08-22）：SiliconFlow 对 batch>=16 的请求会静默返回缺项，
    embed_server._embed_siliconflow 兜底把缺项填成零向量 → 污染 memory_vectors。
    这里在收到响应后立即校验「无零/退化向量」，遇退化整体重试，绝不把零向量交回上层。
    """
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
            elif any(_is_degenerate(v) for v in vecs):
                last_err = "含零/退化向量(高负载静默退化), 重试"
            else:
                return vecs
        except Exception as e:
            last_err = str(e)
        time.sleep(2 * (attempt + 1))
    print(f"  [FAIL] batch {len(texts)} 重试{retries}次失败: {last_err}")
    return None


def main():
    ap = argparse.ArgumentParser()
    # 2026-08-22：默认 16 会触发 SiliconFlow 批请求缺项→零向量污染；降到 8（实测干净）。
    ap.add_argument("--batch", type=int, default=8)
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

    # 安全闸（2026-08-06 教训：连续高负载下嵌入服务可能静默退化，返回错误向量
    # 导致全量污染。写回前抽样验证「新向量 vs 单条实时嵌入」一致性，均值<0.9 则中止）。
    import random as _rnd
    _rnd.seed(42)
    probe = _rnd.sample(new_vectors, min(20, len(new_vectors)))
    bad = 0
    for mid, v in probe:
        content = next((c for m, c in rows if m == mid), "")
        if not content:
            continue
        vecs = embed_batch([content[:1500]])
        if vecs is None:
            bad += 1
            continue
        lv = np.array(vecs[0], dtype=np.float32)
        sv = np.array(v, dtype=np.float32)
        s = float(sv @ lv / (np.linalg.norm(sv) * np.linalg.norm(lv) + 1e-9))
        if s < 0.9:
            bad += 1
    if bad > max(1, len(probe) * 0.2):
        print(f"❌ 安全闸拦截: 抽样 {len(probe)} 条中 {bad} 条一致性<0.9 —— "
              f"嵌入服务输出不可信，中止写回（数据未改动）")
        sys.exit(3)
    print(f"✅ 安全闸通过: 抽样 {len(probe)} 条, {len(probe)-bad} 条一致性>=0.9")

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
