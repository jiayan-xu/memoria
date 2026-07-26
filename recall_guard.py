#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生产召回回归守护（cold-state recall@10 监控）。

目的：每次跑都能稳定检测「新部署 / 配置改动是否悄悄劣化了真实问答召回」。
做法：复位 F1b 频率预热（reset cold）→ 对 58 条问句金标准跑 memory_recall(max_results=100)
      → 算 @10 命中率与 semantic 通道直接命中，与基线比对，低于阈值则写 WARNING 并落趋势 CSV。

与 diag_search_depth.py 的关系：本脚本是「可定时、可报警、可落趋势」的封装版，
复用同一份金标准（eval/hyde_queries.json）与同一套测量口径，输出更适合自动化消费。

用法（在 memoria-open 目录）：
  python recall_guard.py                 # 复位冷态 + 测量 + 落趋势 + 报警
  python recall_guard.py --no-reset     # 不复位（测生产热态，噪声更大）
  python recall_guard.py --json-out x.json  # 另存明细

退出码：0=达标 / 1=召回劣化(低于阈值) / 2=运行错误(无法连接 memoria / 缺金标准)
依赖：numpy（系统 Python 3.14 已带）、urllib、sqlite3。
"""
import sqlite3, json, urllib.request, os, array, math, time, argparse, sys

HERE = os.path.dirname(os.path.abspath(__file__))
DB = os.environ.get("MEMORIA_DB_PATH",
                    os.path.join(HERE, "..", "memoria", "data", "memoria.db"))


def resolve_env_path():
    """定位运行时 .env（避免硬编码用户名路径）：优先 MEMORIA_ENV_PATH 环境变量，
    其次同级的 ../memoria/.env（本机运行时镜像），最后 ./env。"""
    if os.environ.get("MEMORIA_ENV_PATH"):
        return os.environ["MEMORIA_ENV_PATH"]
    for cand in (os.path.join(HERE, "..", "memoria", ".env"),
                 os.path.join(HERE, ".env")):
        if os.path.exists(cand):
            return cand
    return os.path.join(HERE, ".env")


ENV_PATH = resolve_env_path()
MCP_URL = "http://127.0.0.1:9003/mcp"
EMB_URL = "http://127.0.0.1:8777/embed"
NS = "agent/xujiayan"
POOL = 100
GOLD = os.path.join(HERE, "eval", "hyde_queries.json")
TREND = os.environ.get("RECALL_TREND_CSV", os.path.join(HERE, "recall_trend.csv"))

# 金标准基线（2026-07-26 主通道保底 + 软信号降权修复后冷态实测：42/58）
BASELINE_AT10 = 42 / 58          # ≈ 0.7241
WARN_AT10 = 0.65                 # 低于此即视为回归（相对基线 -7.4pp 容差）


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


def reset_cold(db):
    for attempt in range(5):
        try:
            con = sqlite3.connect(db, timeout=30)
            con.execute("PRAGMA busy_timeout=30000")
            n = con.execute(
                "UPDATE memories SET access_count=0, last_recalled=NULL").rowcount
            con.commit()
            con.close()
            print(f"[guard] cold reset OK: {n} rows")
            return True
        except sqlite3.OperationalError as e:
            print(f"[guard] cold reset busy (attempt {attempt+1}): {e}; retry")
            time.sleep(1)
    print("[guard] cold reset FAILED after retries")
    return False


def embed(text):
    body = {"texts": [text]}
    req = urllib.request.Request(EMB_URL, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"},
                                 method="POST")
    d = json.loads(urllib.request.urlopen(req, timeout=60).read())
    return np.array(d["embeddings"][0], dtype=np.float32)


def recall_pool(query, admin_key):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_recall",
                          "arguments": {"query": query, "namespace": NS,
                                        "max_results": POOL}}}
    req = urllib.request.Request(MCP_URL, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json",
                                          "X-Agent-Id": "admin",
                                          "X-Agent-Key": admin_key}, method="POST")
    d = json.loads(urllib.request.urlopen(req, timeout=90).read())
    return d["result"]["content"][0]["text"]


import numpy as np


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-reset", action="store_true",
                    help="不复位冷态（测生产热态，噪声更大）")
    ap.add_argument("--json-out", default=None, help="另存明细 JSON 路径")
    args = ap.parse_args()

    if not os.path.exists(GOLD):
        print(f"[guard] FATAL: 金标准缺失 {GOLD}（需要 eval/hyde_queries.json）")
        sys.exit(2)
    admin_key = load_env(ENV_PATH).get("MEMORIA_ADMIN_KEY", "")

    # 载入命名空间全部向量（算 gold 真实余弦排名，用于 (A) 候选漏召诊断）
    try:
        con = sqlite3.connect(DB, timeout=30)
        con.execute("PRAGMA busy_timeout=30000")
        cur = con.cursor()
        cur.execute("""SELECT v.id, v.vector FROM memory_vectors v
                       JOIN memories m ON v.id=m.id WHERE m.namespace=?""", (NS,))
        ids, vecs = [], []
        for mid, blob in cur.fetchall():
            a = array.array("f"); a.frombytes(blob)
            ids.append(mid); vecs.append(list(a))
        con.close()
    except Exception as e:
        print(f"[guard] FATAL: 无法读取 DB 向量: {e}")
        sys.exit(2)
    if not vecs:
        print("[guard] FATAL: 命名空间无向量")
        sys.exit(2)
    M = np.array(vecs, dtype=np.float32)
    norms = np.linalg.norm(M, axis=1, keepdims=True)
    norms[norms == 0] = 1.0
    M = M / norms
    id2row = {mid: i for i, mid in enumerate(ids)}

    if not args.no_reset:
        reset_cold(DB)

    bench = json.load(open(GOLD, encoding="utf-8"))["items"]
    rec = {"n": 0, "missing_vec": 0, "rank_le100": 0, "A_dropped": 0,
           "B_pool_low": 0, "pool_top10": 0, "sem_channel_top10": 0,
           "gold_not_in_pool": 0}
    ranks = []
    detail = []
    t0 = time.time()
    for it in bench:
        mid, q = it["id"], it["query"]
        if mid not in id2row:
            rec["missing_vec"] += 1
            continue
        gi = id2row[mid]
        try:
            vq = embed(q)
        except Exception as e:
            print("  embed FAIL:", e); continue
        nrm = np.linalg.norm(vq)
        if nrm == 0:
            continue
        vq = vq / nrm
        cos = M @ vq
        gold_cos = float(cos[gi])
        gold_rank = 1 + int((cos > gold_cos).sum())
        ranks.append(gold_rank)
        try:
            raw = recall_pool(q, admin_key)
        except Exception as e:
            print("  recall FAIL:", e); continue
        data = json.loads(raw)
        pool = data.get("results", [])
        pool_ids = [x.get("memory_id") for x in pool]
        in_pool = mid in pool_ids
        pool_pos = pool_ids.index(mid) + 1 if in_pool else None
        rec["n"] += 1
        if gold_rank <= 100:
            rec["rank_le100"] += 1
        if not in_pool:
            rec["gold_not_in_pool"] += 1
            if gold_rank <= 100:
                rec["A_dropped"] += 1
        else:
            if pool_pos <= 10:
                rec["pool_top10"] += 1
                item = next(x for x in pool if x.get("memory_id") == mid)
                sigs = item.get("signal_scores") or []
                chans = [s[0] for s in sigs] if sigs else []
                pc = item.get("primary_channel")
                if "semantic" in chans or pc == "semantic":
                    rec["sem_channel_top10"] += 1
            else:
                rec["B_pool_low"] += 1
        detail.append({"id": mid, "gold_rank": gold_rank, "in_pool": in_pool,
                       "pool_pos": pool_pos})
        if rec["n"] % 20 == 0:
            print(f"  ...{rec['n']} ({time.time()-t0:.0f}s)")

    at10 = rec["pool_top10"] / rec["n"] if rec["n"] else 0.0
    sem10 = rec["sem_channel_top10"] / rec["n"] if rec["n"] else 0.0
    a_drop = rec["A_dropped"] / rec["rank_le100"] if rec["rank_le100"] else 0.0

    print("\n================ 召回回归守护 ================")
    print(f"有效样本 n={rec['n']}（缺失向量 {rec['missing_vec']}）")
    print(f"@10 命中率     : {at10*100:.1f}%   (基线 {BASELINE_AT10*100:.1f}%, 报警阈值 {WARN_AT10*100:.0f}%)")
    print(f"semantic 直击 : {sem10*100:.1f}%")
    print(f"(A) 候选漏召   : {rec['A_dropped']}/{rec['rank_le100']} ({a_drop*100:.1f}%)")
    print(f"(B) 池内>10    : {rec['B_pool_low']}")
    print(f"完全不在池     : {rec['gold_not_in_pool']}/{rec['n']}")

    status = "OK" if at10 >= WARN_AT10 else "REGRESSION"
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    trend_line = (f"{ts},{at10:.4f},{sem10:.4f},{rec['A_dropped']},"
                  f"{rec['rank_le100']},{rec['gold_not_in_pool']},{status}\n")
    try:
        with open(TREND, "a", encoding="utf-8") as f:
            if os.path.getsize(TREND) == 0:
                f.write("timestamp,at10,sem_top10,A_dropped,rank_le100,"
                        "not_in_pool,status\n")
            f.write(trend_line)
        print(f"趋势已追加 -> {TREND}")
    except Exception as e:
        print(f"[guard] 趋势写入失败: {e}")

    if args.json_out:
        json.dump(detail, open(args.json_out, "w", encoding="utf-8"),
                  ensure_ascii=False, indent=2)
        print(f"明细 -> {args.json_out}")

    if status == "REGRESSION":
        print(f"\n⚠️ 召回回归预警：@10={at10*100:.1f}% 低于阈值 {WARN_AT10*100:.0f}%"
              f"（基线 {BASELINE_AT10*100:.1f}%）")
        sys.exit(1)
    print("\n✅ 召回达标")
    sys.exit(0)


if __name__ == "__main__":
    main()
