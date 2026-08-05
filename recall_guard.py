#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生产召回回归守护（动态抽样 recall@5 监控，模型空间无关）。

目的：每次跑都能稳定检测「新部署 / 配置改动 / 索引污染是否悄悄劣化了真实问答召回」。

做法（模型空间无关，不再依赖冻结的 text2vec 金标准）：
  1. 从生产库按命名空间随机抽样 N 条记忆（默认 40）；
  2. 对每条构造两种 query：
       - 完整内容查询：用记忆原文查询（self-recall，检索上限，对应真实值 92.5%）
       - 关键词查询  ：用提取的核心关键词查询（模拟真实短词查询，对应真实值 52.5%）
  3. 各跑 memory_recall(max_results=K)，检查该记忆自身 id 是否进入 top-5；
  4. 分别计算 recall@5（完整内容 / 关键词），与基线比对，低于阈值写 WARNING 并落趋势 CSV。

为什么重订（2026-08-02）：
  旧版用 eval/hyde_queries.json 58 条问句金标准，其 gold id 当初用 text2vec 嵌入定位，
  换 Qwen3-VL 后向量近邻整体移位，映射错位 → 测的是错的东西（27.6% 是假象）。
  旧版还把 rerank 服务降级（纯余弦）误报成"召回回归"。
  新版动态抽样 + 原文/关键词查询，不依赖任何冻结模型空间，换 embedding 也有效。

用法（在 memoria-open 目录）：
  python recall_guard.py                 # 测生产热态（默认不复位，与 92.5% 基线口径一致）+ 落趋势 + 报警
  python recall_guard.py --reset        # 复位冷态再测（清零 access_count，调试用）
  python recall_guard.py --sample-n 40  # 抽样条数
  python recall_guard.py --json-out x.json  # 另存明细

退出码：0=达标 / 1=召回劣化(低于阈值) / 2=运行错误(无法连接 memoria / 库无数据)
依赖：urllib、sqlite3、re（标准库，无需 numpy）。
"""
import sqlite3, json, urllib.request, os, re, time, argparse, sys

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
NS = os.environ.get("MEMORIA_GUARD_NS", "agent/xujiayan")
K = 10                       # 拉取条数（判定 top-5 命中，多取几条便于诊断）
SAMPLE_N = int(os.environ.get("RECALL_SAMPLE_N", "40"))
TREND = os.environ.get("RECALL_TREND_CSV", os.path.join(HERE, "recall_trend.csv"))
EXPECTED_HEADER = "timestamp,full_r5,kw_r5,n_full,n_kw,status\n"

# 双口径基线/阈值 —— 2026-08-02 重订，基于清理 LoCoMo 后 Qwen3-VL 生产实测（40 抽样 @5）
# 完整内容查询召回@5：用记忆原文查自己，检索上限；实测 37/40 = 92.5%
FULL_BASELINE = 0.925
FULL_WARN     = 0.85          # 跌破≈-7.5pp，embedding/索引真故障
# 关键词查询召回@5：模拟真实短词查询，更难；实测 21/40 = 52.5%
KW_BASELINE   = 0.525
KW_WARN       = 0.40          # 关键词口径固有波动大，阈值放宽至 -12.5pp


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


def append_trend(path, line):
    """安全追加趋势 CSV。

    若已存在文件且首行 header 与 EXPECTED_HEADER 不一致（legacy / 版本变更导致的
    旧 7 列结构），先备份旧文件为 <path>.legacy.<ts> 再重建，避免新 6 列行被追加进
    旧结构而静默破坏列对齐，使长期趋势对比失效。"""
    need_header = True
    if os.path.exists(path) and os.path.getsize(path) > 0:
        with open(path, encoding="utf-8") as f:
            first = f.readline()
        if first == EXPECTED_HEADER:
            need_header = False
        else:
            bak = f"{path}.legacy.{time.strftime('%Y%m%d%H%M%S')}"
            try:
                os.replace(path, bak)
                print(f"[guard] 检测到不兼容 header，已备份旧趋势 -> {bak}")
            except OSError as e:
                print(f"[guard] legacy 备份失败（继续覆盖）: {e}")
    with open(path, "a", encoding="utf-8") as f:
        if need_header:
            f.write(EXPECTED_HEADER)
        f.write(line)


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


def recall_topk(query, admin_key, k=K):
    payload = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "memory_recall",
                          "arguments": {"query": query, "namespace": NS,
                                        "max_results": k}}}
    req = urllib.request.Request(MCP_URL, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json",
                                          "X-Agent-Id": "admin",
                                          "X-Agent-Key": admin_key}, method="POST")
    d = json.loads(urllib.request.urlopen(req, timeout=90).read())
    txt = d["result"]["content"][0]["text"]
    return json.loads(txt).get("results", [])


def extract_keywords(content, tags):
    """关键词模拟查询构造：tags 优先（记忆自带关键词），否则取内容前 40 字（短查询模拟）。
    这是启发式——关键词口径的精确复现依赖记忆 tags 质量；若需精确复现可后续固化抽样语料。
    tags 在库中存为 JSON 数组字符串（如 '["a","b"]' 或 '["a" "b"]'），需先解析去引号。"""
    kw = []
    if tags:
        t = tags.strip()
        if t.startswith("["):
            try:
                arr = json.loads(t)
                if isinstance(arr, list):
                    kw = [str(x).strip() for x in arr if str(x).strip()]
            except Exception:
                kw = [x.strip("[]\"' ") for x in re.split(r"[;,，、\s]+", t)
                      if x.strip("[]\"' ")]
        else:
            kw = [x.strip() for x in re.split(r"[;,，、\s]+", t) if x.strip()]
    if kw:
        return " ".join(kw[:8])
    c = re.sub(r"\s+", " ", (content or "").strip())
    return c[:40]


def sample_memories(db, n):
    con = sqlite3.connect(db, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    cur = con.cursor()
    cur.execute(
        """SELECT id, content, tags FROM memories
           WHERE namespace=? AND length(coalesce(content,''))>0
           ORDER BY RANDOM() LIMIT ?""", (NS, n))
    rows = cur.fetchall()
    con.close()
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reset", action="store_true",
                    help="复位冷态（清零 access_count/last_recalled）再测；"
                         "默认不复位，以反映生产真实热态召回（与 92.5%% 基线口径一致）")
    ap.add_argument("--json-out", default=None, help="另存明细 JSON 路径")
    ap.add_argument("--sample-n", type=int, default=SAMPLE_N,
                    help="抽样条数（默认 40）")
    args = ap.parse_args()

    admin_key = load_env(ENV_PATH).get("MEMORIA_ADMIN_KEY", "")
    rows = sample_memories(DB, args.sample_n)
    if not rows:
        print(f"[guard] FATAL: 命名空间 {NS} 无可用记忆（或库不可达 {DB}）")
        sys.exit(2)

    if args.reset:
        reset_cold(DB)

    n_full = n_kw = 0
    n_attempt_full = n_attempt_kw = 0   # 含失败次数的尝试计数，用于区分「全失败(运行错误)」与「召回劣化」
    hit_full = hit_kw = 0
    hit_full10 = 0
    detail = []
    t0 = time.time()
    for mid, content, tags in rows:
        n_attempt_full += 1
        # 完整内容查询（self-recall 检索上限）
        full_q = (content or "")[:4000]
        try:
            top = recall_topk(full_q, admin_key, K)
        except Exception as e:
            print("  recall FAIL:", e); continue
        top_ids = [x.get("memory_id") for x in top]
        full_rank = top_ids.index(mid) + 1 if mid in top_ids else None
        in_full = full_rank is not None and full_rank <= 5
        n_full += 1
        if in_full:
            hit_full += 1
        if full_rank is not None and full_rank <= 10:
            hit_full10 += 1
        # 关键词查询（真实短词口径）
        kw_q = extract_keywords(content, tags)
        if not kw_q:
            continue
        n_attempt_kw += 1
        try:
            top2 = recall_topk(kw_q, admin_key, K)
        except Exception as e:
            print("  recall FAIL:", e); continue
        top2_ids = [x.get("memory_id") for x in top2]
        kw_rank = top2_ids.index(mid) + 1 if mid in top2_ids else None
        in_kw = kw_rank is not None and kw_rank <= 5
        n_kw += 1
        if in_kw:
            hit_kw += 1
        detail.append({"id": mid, "kw_query": kw_q,
                       "full_rank": full_rank, "kw_rank": kw_rank})
        if n_full % 20 == 0:
            print(f"  ...{n_full} ({time.time()-t0:.0f}s)")

    full_r5 = hit_full / n_full if n_full else 0.0
    full_r10 = hit_full10 / n_full if n_full else 0.0
    kw_r5 = hit_kw / n_kw if n_kw else 0.0

    print("\n================ 召回回归守护（动态抽样 @5） ================")
    print(f"抽样命名空间 : {NS}")
    print(f"样本数       : 完整内容 {n_full} / 关键词 {n_kw}")
    print(f"完整内容@5   : {full_r5*100:.1f}%   (基线 {FULL_BASELINE*100:.1f}%, 报警阈值 {FULL_WARN*100:.0f}%)")
    print(f"完整内容@10  : {full_r10*100:.1f}%   (仅诊断用)")
    print(f"关键词@5     : {kw_r5*100:.1f}%   (基线 {KW_BASELINE*100:.1f}%, 报警阈值 {KW_WARN*100:.0f}%)")

    # 双口径分诊：先判检索上限（embedding/索引真故障），再判语义口径（短词召回）
    if full_r5 < FULL_WARN:
        status = "RECALL_DEGRADED"
    elif kw_r5 < KW_WARN:
        status = "SEMANTIC_DEGRADED"
    else:
        status = "OK"
    # 全失败检测：所有 full recall 调用都失败（memoria 不可达/超时）属运行错误，
    # 应判 exit 2（与 docstring 一致），而非因 full_r5=0 误判 exit 1（召回劣化）。
    # 此时也没有有效测量值，不写趋势（避免以 0.0000 污染历史）。
    if n_full == 0:
        print(f"\n❌ GUARD ERROR：命名空间 {NS} 共 {n_attempt_full} 次 full recall 调用全部失败，"
              f"无法连接 memoria（{MCP_URL}）或查询超时——判定为运行错误（exit 2），"
              f"已跳过趋势写入以避免以 0% 污染历史")
        sys.exit(2)

    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    trend_line = (f"{ts},{full_r5:.4f},{kw_r5:.4f},{n_full},{n_kw},{status}\n")
    try:
        append_trend(TREND, trend_line)
        print(f"趋势已追加 -> {TREND}")
    except Exception as e:
        print(f"[guard] 趋势写入失败: {e}")

    if args.json_out:
        json.dump(detail, open(args.json_out, "w", encoding="utf-8"),
                  ensure_ascii=False, indent=2)
        print(f"明细 -> {args.json_out}")

    if status == "OK":
        print("\n✅ 召回达标（完整内容@5 与 关键词@5 均正常）")
        sys.exit(0)
    elif status == "RECALL_DEGRADED":
        print(f"\n⚠️ 召回检索劣化：完整内容@5={full_r5*100:.1f}% 低于阈值 "
              f"{FULL_WARN*100:.0f}%（基线 {FULL_BASELINE*100:.1f}%）—— "
              f"embedding / 索引真故障，记忆无法被自身内容召回")
        sys.exit(1)
    else:  # SEMANTIC_DEGRADED
        print(f"\n⚠️ 语义召回劣化：关键词@5={kw_r5*100:.1f}% 低于阈值 "
              f"{KW_WARN*100:.0f}%（基线 {KW_BASELINE*100:.1f}%）—— "
              f"短词/泛化查询召回下降（embedding 质量或 rerank 异常），"
              f"但完整内容@5={full_r5*100:.1f}% 仍正常")
        sys.exit(1)


if __name__ == "__main__":
    main()
