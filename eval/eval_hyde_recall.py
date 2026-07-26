#!/usr/bin/env python3
"""
HyDE（Hypothetical Document Embeddings）召回增益实证评测。

HyDE 的本质：把「用户问句」用 LLM 改写成「假设答案文档」再嵌入，缩小
「问句 vs 知识库陈述」的措辞 gap。本脚本做的是公平 A/B：
- 同一份「问句式 query → 答案记忆 gold」基准（hyde_queries.json，固定、不入库）。
- 仅切换 memoria 服务端 `MEMORIA_HYDE` 环境变量（OFF vs ON），query 输入完全相同。
- 跑真实 memory_recall 全管线（向量 + 图 2-hop + cross-encoder 重排，与生产一致），
  对比 recall@k，得出 HyDE 的真实端到端增益。

用法：
  # 1) 构造基准（用硅流 LLM 把答案型记忆改写成自然问句，固定存盘）
  python eval/eval_hyde_recall.py --build --out eval/hyde_off.json
  # 2) 当前（HyDE 关）先跑一次基线
  #    —— 上面 --build 已顺带跑了 OFF，结果在 hyde_off.json
  # 3) 停 memoria → 注入 MEMORIA_HYDE=1 重启 → 跑 ON
  python eval/eval_hyde_recall.py --out eval/hyde_on.json
  # 4) 对比
  python eval/eval_hyde_recall.py --compare eval/hyde_off.json eval/hyde_on.json

P0：禁硬编码用户名绝对路径；DB/ENV/KEY 走 env 或 expanduser。
"""
import json, os, sys, argparse, urllib.request, urllib.error, sqlite3, time, random

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH_PATH = os.path.join(HERE, "hyde_queries.json")
NS = "agent/xujiayan"
KS = [1, 3, 5, 10]

# 走 memoria 运行时副本的 .env（不打印明文）
MEMORIA_ENV = os.path.join(os.path.expanduser("~/.qclaw/workspace/memoria"), ".env")
# 实时库（与 tray 注入 MEMORIA_DB_PATH 一致）
DB_PATH = os.environ.get(
    "MEMORIA_DB_PATH",
    os.path.expanduser("~/.qclaw/workspace/memoria/data/memoria.db"),
)
MCP_BASE = os.environ.get("MEMORIA_MCP_BASE", "http://127.0.0.1:9003")

# 硅流 chat（与 embed_server.py 的 HyDE 同后端，用同一模型保证可比）
SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "")
SF_CHAT_API = "https://api.siliconflow.cn/v1/chat/completions"
HYDE_GEN_MODEL = "Qwen/Qwen2.5-7B-Instruct"


def load_admin_key():
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


# ── 基准构造：把答案型记忆改写成自然问句 ──
def sf_generate_question(content: str) -> str:
    """用硅流 LLM 把一条知识库事实改写成用户口吻的自然问句（刻意换措辞，制造 lexical gap）。"""
    if not SF_KEY:
        raise RuntimeError("SILICONFLOW_API_KEY 未设置，无法生成问句")
    import requests
    sys_prompt = (
        "你是一个检索评测构造器。给定一条知识库中的事实/答案文本，请写一句"
        "「用户会怎么提问来找回这条信息」的自然问句。要求：\n"
        "1) 用与原文不同的词汇和措辞（不要照搬原文术语，模拟用户不知道精确术语）；\n"
        "2) 问句要具体、可被该事实回答；\n"
        "3) 只输出这一句问句本身，不要解释、不要引号、不要编号。\n"
        "若原文明显不是可检索的事实（如纯代码报错、乱码），输出空字符串。"
    )
    body = {
        "model": HYDE_GEN_MODEL,
        "messages": [
            {"role": "system", "content": sys_prompt},
            {"role": "user", "content": content},
        ],
        "max_tokens": 120,
        "temperature": 0.7,
        "stream": False,
    }
    headers = {"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"}
    for attempt in range(3):
        try:
            r = requests.post(SF_CHAT_API, headers=headers, json=body, timeout=30)
            if r.status_code == 200:
                return r.json()["choices"][0]["message"]["content"].strip().strip('"').strip("'")
            elif r.status_code in (429, 503, 504):
                time.sleep(min(2 ** attempt, 10)); continue
            else:
                raise RuntimeError(f"SF chat {r.status_code}: {r.text[:200]}")
        except Exception as e:
            time.sleep(min(2 ** attempt, 10))
            if attempt == 2:
                raise RuntimeError(f"SF chat 失败: {e}")
    return ""


def build_bench(n=60):
    con = sqlite3.connect(DB_PATH)
    cur = con.cursor()
    NOW = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
    # 候选：agent/xujiayan 下、有效、未被取代、长度适中、含中文的自然语言事实
    cur.execute("""
        SELECT id, content FROM memories
        WHERE namespace=? AND superseded_by IS NULL
          AND (valid_to IS NULL OR valid_to > ?)
          AND length(content) BETWEEN 20 AND 360
    """, (NS, NOW))
    rows = cur.fetchall()
    con.close()
    # 过滤：含足够中文字符（避免纯代码/URL）
    def cjk_count(s):
        return sum(1 for ch in s if '一' <= ch <= '鿿')
    cands = [(mid, c) for mid, c in rows if cjk_count(c) >= 8]
    random.seed(20260726)
    random.shuffle(cands)
    # 模型常把「非事实」指令误答成字面哨兵串，必须滤掉
    SENTINELS = {"空字符串", "无", "无内容", "无相关", "无可用", "无问题",
                 "未提供", "同上", "null", "n/a", "na", "none", "无答案"}
    items, used = [], 0
    print(f"候选（含中文、有效、长度适中）: {len(cands)}，目标生成 {n} 条问句")
    for mid, c in cands:
        if used >= n:
            break
        try:
            q = sf_generate_question(c)
        except Exception as e:
            print(f"  [warn] 生成失败 {mid[:8]}: {e}")
            continue
        q = q.strip().strip('`').strip()
        if not q or len(q) < 6:
            continue
        if q in SENTINELS:
            continue
        if q == c.strip():
            continue
        items.append({"id": mid, "content": c, "query": q, "type": "hyde_nl"})
        used += 1
        if used % 10 == 0:
            print(f"  ...{used}/{n}")
    if not items:
        raise SystemExit("未生成任何问句（检查 SF_KEY / 网络）")
    with open(BENCH_PATH, "w", encoding="utf-8") as f:
        json.dump({"namespace": NS, "n": len(items), "items": items}, f,
                  ensure_ascii=False, indent=2)
    print(f"基准已写 {BENCH_PATH}（n={len(items)}）")
    return items


# ── 召回：走真实 memory_recall 全管线 ──
def mcp_recall(base, admin_key, query, max_results=15):
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
        headers={"Content-Type": "application/json",
                 "X-Agent-Id": "admin", "X-Agent-Key": admin_key or ""},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        resp = json.loads(r.read().decode("utf-8"))
    try:
        text = resp["result"]["content"][0]["text"]
        data = json.loads(text)
        out = []
        for x in data.get("results", []):
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


def run_eval(items, admin_key, tag):
    ks = KS
    hits = {t: {k: 0 for k in ks} for t in ["hybrid", "semantic"]}
    per = []
    for it in items:
        qid, q, gold = it["id"], it["query"], {it["id"]}
        got = mcp_recall(MCP_BASE, admin_key, q, max_results=15)
        best = None
        for idx, (mid, _) in enumerate(got):
            if mid in gold:
                best = idx + 1
                break
        for k in ks:
            topk = got[:k]
            sem_hit = any(("semantic" in ch) for (mid, ch) in topk if mid in gold)
            hyb_hit = any(mid in gold for (mid, _) in topk)
            hits["hybrid"][k] += (1 if hyb_hit else 0)
            hits["semantic"][k] += (1 if sem_hit else 0)
        per.append({"id": qid, "query": q, "best_rank": best, "ngot": len(got)})
    n = len(items)
    summary = {
        "tag": tag, "namespace": NS, "n": n, "ks": ks,
        "hybrid": {k: round(hits["hybrid"][k] / n * 100, 1) for k in ks},
        "semantic": {k: round(hits["semantic"][k] / n * 100, 1) for k in ks},
    }
    return summary, per


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="store_true", help="先构造基准（若缺失）")
    ap.add_argument("--out", default=os.path.join(HERE, "hyde_result.json"))
    ap.add_argument("--compare", nargs=2, metavar=("OFF", "ON"))
    args = ap.parse_args()

    if args.compare:
        a = json.load(open(args.compare[0], encoding="utf-8"))
        b = json.load(open(args.compare[1], encoding="utf-8"))
        print(f"=== HyDE 增益对比（n={a['n']}, namespace={a['namespace']}）===")
        print(f"{'k':>4} | {'OFF hybrid':>11} | {'ON hybrid':>10} | {'Δhybrid':>8} | {'OFF sem':>9} | {'ON sem':>8} | {'Δsem':>7}")
        for k in KS:
            oh = a["hybrid"][str(k)] if str(k) in a["hybrid"] else a["hybrid"][k]
            on = b["hybrid"][str(k)] if str(k) in b["hybrid"] else b["hybrid"][k]
            os_ = a["semantic"][str(k)] if str(k) in a["semantic"] else a["semantic"][k]
            ons = b["semantic"][str(k)] if str(k) in b["semantic"] else b["semantic"][k]
            print(f"{k:>4} | {oh:>9.1f}% | {on:>8.1f}% | {(on-oh):>+6.1f}pp | {os_:>7.1f}% | {ons:>6.1f}% | {(ons-os_):>+5.1f}pp")
        return

    if args.build or not os.path.exists(BENCH_PATH):
        items = build_bench(n=60)
    else:
        items = json.load(open(BENCH_PATH, encoding="utf-8"))["items"]

    admin_key = load_admin_key()
    if not admin_key:
        print("WARN: MEMORIA_ADMIN_KEY 未取到，可能鉴权失败", file=sys.stderr)
    # tag 取自 out 文件名，便于区分 off/on
    tag = os.path.splitext(os.path.basename(args.out))[0]
    summary, per = run_eval(items, admin_key, tag)
    out = {"summary": summary, "per_query": per}
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)
    print(f"\n=== HyDE 评测 [{tag}]（n={summary['n']}）===")
    print("hybrid  recall@" + "/".join(map(str, KS)) + ": " +
          " / ".join(f"{k}:{summary['hybrid'][k]}%" for k in KS))
    print("semantic recall@" + "/".join(map(str, KS)) + ": " +
          " / ".join(f"{k}:{summary['semantic'][k]}%" for k in KS))
    print(f"\n结果写 {args.out}")


if __name__ == "__main__":
    main()
