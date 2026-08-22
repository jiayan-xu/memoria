#!/usr/bin/env python3
"""
Memoria 自然语言召回评测 (模型无关)。

走真实 memory_recall A+C 管线，对 nl_recall_bench.json 里每条用户口吻 query，
检查其标注的 ground-truth 记忆是否进 top-k。适合在切换嵌入模型(如 text2vec->bge-m3)
+ 全量重嵌后跑同一脚本，做公平 A/B。

用法:
  python eval/eval_nl_recall.py [--maxk 10] [--base http://127.0.0.1:9003]
"""
import json, os, sys, argparse, urllib.request, urllib.error

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.join(HERE, "nl_recall_bench.json")
# R0 封口4 后密钥迁至 ~/.svc-secrets/memoria.env；回退旧路径兼容
MEMORIA_ENV = os.path.join(os.path.expanduser("~/.svc-secrets"), "memoria.env")
if not os.path.exists(MEMORIA_ENV):
    MEMORIA_ENV = os.path.join(os.path.expanduser("~/.qclaw/workspace/memoria"), ".env")
NS = "agent/xujiayan"

def load_admin_key():
    # 读 memoria\.env 的 MEMORIA_ADMIN_KEY，不打印明文
    key = None
    try:
        with open(MEMORIA_ENV, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                if line.startswith("MEMORIA_ADMIN_KEY="):
                    key = line.split("=", 1)[1].strip().strip('"').strip("'")
                    break
    except FileNotFoundError:
        pass
    return key

def mcp_recall(base, admin_key, query, max_results=10):
    payload = {
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "memory_recall",
            "arguments": {"query": query, "namespace": NS, "max_results": max_results},
        },
    }
    req = urllib.request.Request(
        base + "/mcp",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "X-Agent-Id": "admin",
            "X-Agent-Key": admin_key or "",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        resp = json.loads(r.read().decode("utf-8"))
    # 返回 [(memory_id, set(channels))] —— channels 来自 signal_scores
    try:
        text = resp["result"]["content"][0]["text"]
        data = json.loads(text)
        results = data.get("results", [])
        out = []
        for x in results:
            mid = x.get("memory_id")
            if not mid:
                continue
            ch = set()
            for s in x.get("signal_scores", []) or []:
                if isinstance(s, list) and s:
                    ch.add(s[0])
            out.append((mid, ch))
        return out
    except Exception:
        return []

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--maxk", type=int, default=10)
    ap.add_argument("--base", default="http://127.0.0.1:9003")
    args = ap.parse_args()

    with open(BENCH, encoding="utf-8") as f:
        bench = json.load(f)
    items = bench["items"]
    admin_key = load_admin_key()
    if not admin_key:
        print("WARN: MEMORIA_ADMIN_KEY 未取到，可能鉴权失败", file=sys.stderr)

    ks = [1, 3, 5, 10] if args.maxk >= 10 else [1, 3, 5, args.maxk]
    # 三通道命中计数：hybrid=任意通道命中；semantic=仅 semantic 通道命中；keyword=仅 keyword 通道命中
    hits = {t: {k: 0 for k in ks} for t in ["hybrid", "semantic", "keyword"]}
    n = 0
    per_query = []

    for it in items:
        qid, q, rel, typ = it["id"], it["query"], set(it["relevant_ids"]), it["type"]
        n += 1
        got = mcp_recall(args.base, admin_key, q, max_results=args.maxk)  # [(mid, ch)]
        # 各通道在 top-k 命中的 gt 集合（按 k 独立计算，保证 recall@k 单调且可分）
        best = None
        for idx, (mid, _) in enumerate(got):
            if mid in rel:
                best = idx + 1
                break
        for k in ks:
            sem_hit, kw_hit, hyb_hit = set(), set(), set()
            topk = got[:k]
            for rid in rel:
                for (mid, ch) in topk:
                    if mid == rid:
                        if "semantic" in ch:
                            sem_hit.add(rid)
                        if "keyword" in ch:
                            kw_hit.add(rid)
                        hyb_hit.add(rid)
                        break
            hits["hybrid"][k] += (1 if hyb_hit else 0)
            hits["semantic"][k] += (1 if sem_hit else 0)
            hits["keyword"][k] += (1 if kw_hit else 0)
        per_query.append((qid, typ, len(rel), best, len(got),
                          "sem" if sem_hit else "-", "kw" if kw_hit else "-"))

    print(f"=== NL Recall Bench (namespace={NS}, n={n}, 权重 rrf/sem/kw=0.3/0.2/0.5, 模型=Qwen3-VL-8B/1024) ===")
    for t, label in [("hybrid", "整体/hybrid"), ("semantic", "semantic"), ("keyword", "keyword")]:
        print(f"{label:14} recall@{'/'.join(map(str,ks))}: " +
              " / ".join(f"{k}:{hits[t][k]/n*100:.1f}%" for k in ks))
    print("\n逐条 (sem=语义命中 kw=关键词命中, - = 未命中):")
    for qid, typ, nrel, best, ngot, sem, kw in per_query:
        mark = "OK" if best else "MISS"
        print(f"  [{mark}] {qid} {typ:8} best={best} got={ngot} sem={sem} kw={kw}")

    summary = {
        "n": n, "ks": ks, "model": "Qwen3-VL-8B/1024",
        "hybrid": {k: round(hits["hybrid"][k]/n*100, 1) for k in ks},
        "semantic": {k: round(hits["semantic"][k]/n*100, 1) for k in ks},
        "keyword": {k: round(hits["keyword"][k]/n*100, 1) for k in ks},
    }
    out = os.path.join(HERE, "nl_recall_result.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    print(f"\n摘要写入 {out}")

if __name__ == "__main__":
    main()
