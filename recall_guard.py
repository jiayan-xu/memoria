#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生产召回回归守护（动态抽样 recall@5 监控，模型空间无关）。

目的：每次跑都能稳定检测「新部署 / 配置改动 / 索引污染是否悄悄劣化了真实问答召回」。

做法（模型空间无关，不再依赖冻结的 text2vec 金标准）：
  1. 从生产库按命名空间、tags 类型分层随机抽样 N 条记忆（默认 80）；
  2. 对每条构造三种 query：
      - 完整内容查询：用记忆原文查询（self-recall，检索上限，基线 92.5%）
      - 正文关键词查询：统一用规范化正文前 40 字（唯一关键词告警口径）
      - tags 查询：剔除 signal:/occurred:/auto_consolidated 后诊断，不参与告警
  3. 各跑 memory_recall(max_results=K)，检查该记忆自身 id 是否进入 top-5；
  4. 完整内容 / 正文关键词与基线比对，落趋势 CSV、告警 JSON 和逐条明细。

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
    """定位运行时 env：优先 MEMORIA_ENV_PATH；其次 ~/.svc-secrets/agent-core.env
    （restart.ps1/托盘的权威源）；再其次 memoria.env；最后兼容旧项目内 .env
    （2026-10-19 已退役，仅作兜底）。"""
    if os.environ.get("MEMORIA_ENV_PATH"):
        return os.environ["MEMORIA_ENV_PATH"]
    home = os.environ.get("USERPROFILE") or os.path.expanduser("~")
    for cand in (os.path.join(home, ".svc-secrets", "agent-core.env"),
                 os.path.join(home, ".svc-secrets", "memoria.env"),
                 os.path.join(HERE, "..", "memoria", ".env")):
        if os.path.exists(cand):
            return cand
    return os.path.join(home, ".svc-secrets", "agent-core.env")


ENV_PATH = resolve_env_path()
MCP_URL = os.environ.get("MEMORIA_MCP_URL", "http://127.0.0.1:9003/mcp")
NS = os.environ.get("MEMORIA_GUARD_NS", "agent/xujiayan")
K = 10                       # 拉取条数（判定 top-5 命中，多取几条便于诊断）
# 样本量：2026-10-03 由 40 提到 80。理由——每轮抽取的是**不同记忆**，记忆间召回难度
# 异质性使实际方差远大于二项理论值（实测同一健康实例 3 轮 45%/65%/55%，摆幅 20pp）。
# 40 样本下 n=40,p≈0.55 的二项标准差约 7.9pp，40% 阈值距均值仅 1.9σ，单轮误报率约 3%
# ——2026-10-03 03:01 即因此报出 severity=warn 假阳性。n 翻倍后 σ≈5.6pp、阈值升到 2.7σ，
# 误报率降到约 0.4%，且**不牺牲灵敏度**：真实劣化（如 2026-10-01 嵌入宕机 32.5%）仍被稳定捕获。
# 代价：健康态单轮耗时约 30s→60s，仍远低于 GLOBAL_CAP。
SAMPLE_N = int(os.environ.get("RECALL_SAMPLE_N", "80"))
# 全局墙钟上限：整个守护最多跑 20 分钟。单条 recall 已 timeout=90，但 80×2 条在 memoria
# 极端卡顿时会逼近 4 小时；加此硬上限，超时即提前结束并据已测样本出结论，避免 run 拖死。
# 2026-10-03 由 600 提到 1200：样本量翻倍后，降级态（约 545s/40 条）需约 1090s，
# 若仍用 600s 会在 n≈27 处被截断，样本量优势荡然无存、方差回到调参前水平。
GLOBAL_CAP = int(os.environ.get("RECALL_GLOBAL_CAP", "1200"))
TREND = os.environ.get("RECALL_TREND_CSV", os.path.join(HERE, "recall_trend.csv"))
ALERT = os.environ.get("RECALL_ALERT_JSON", os.path.join(HERE, "recall_alert.json"))
# 连续劣化达到该次数才写 elevated 告警（单次尖峰写 warn，供 memory_ops_status 读取）。
# 2026-10-03 由 2 提到 3。注意：本项**只决定 severity 标签，不影响退出码**——status 一旦
# 跌破阈值就直接 exit 1（见下方分诊逻辑），故它只能软化告警、无法降误报。降误报靠 SAMPLE_N。
CONSEC_WARN = int(os.environ.get("RECALL_CONSEC_WARN", "3"))
EXPECTED_HEADER = "timestamp,full_r5,content_kw_r5,tag_r5,n_full,n_content_kw,n_tag,status\n"

# 双告警口径：完整内容查询测检索上限；正文关键词查询测真实内容检索。
# tags 查询仅作独立诊断，不参与告警——FTS 是否索引 tags 是另一种产品能力，
# 不能与正文关键词召回混算（2026-10-30 关键词@5=27.5% 假回归复盘）。
# 完整内容：2026-08-02 清理 LoCoMo 后 Qwen3-VL 生产实测 37/40 = 92.5%。
FULL_BASELINE = 0.925
FULL_WARN     = 0.85
# 正文关键词：2026-10-30 生产库快照 + 新二进制分层 240 条实测 67.9%，向下取 68% 为基线；
# 阈值 60% 留约 8pp 缓冲（分层后 σ 已显著低于旧的动态全随机抽样）。
CONTENT_KW_BASELINE = 0.68
CONTENT_KW_WARN     = 0.60


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
    旧 6/7 列结构），先备份旧文件为 <path>.legacy.<ts> 再重建，避免新 8 列行被追加进
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


def _results_of(inner):
    """双 schema 兼容解析 results（2026-10-13，与 scripts/retrieval_canary.py 同款）。

    历史：`results` 曾挂在 `recall` 子对象下，后挪到顶层。单 schema 解析在跨构建
    调用时会把「服务器正常返回（fused=5）」误读成「全空」，把诊断方向带偏成
    「环境毒化 / 索引损坏」——2026-10-13 事故复盘时真实发生过（连发 12 次全绿
    + raw 复核才戳穿）。两级都试，缺失即 []。
    """
    if not isinstance(inner, dict):
        return []
    r = inner.get("results")
    if r is None:
        rec = inner.get("recall")
        if isinstance(rec, dict):
            r = rec.get("results")
    return r if isinstance(r, list) else []


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
    txt = "".join(c.get("text", "")
                  for c in (d.get("result") or {}).get("content", []))
    inner = json.loads(txt) if txt.strip().startswith(("{", "[")) else {}
    return _results_of(inner)


def tag_kind(tags):
    """守护分层类型：空 tags / 自动整合 tags / 普通非空 tags。"""
    t = (tags or "").strip()
    if not t or t == "[]":
        return "empty"
    return "auto_consolidated" if "auto_consolidated" in t else "normal"


def parse_tags(tags):
    """解析库内 JSON/分隔符 tags，保留原顺序并去除空项。"""
    if not tags:
        return []
    t = tags.strip()
    if t.startswith("["):
        try:
            arr = json.loads(t)
            if isinstance(arr, list):
                return [str(x).strip() for x in arr if str(x).strip()]
        except Exception:
            pass
        return [x.strip("[]\"' ") for x in re.split(r"[;,，、\s]+", t)
                if x.strip("[]\"' ")]
    return [x.strip() for x in re.split(r"[;,，、\s]+", t) if x.strip()]


def extract_content_query(content):
    """正文关键词查询：统一取规范化正文前 40 字，不再受 tags 质量影响。"""
    c = re.sub(r"\s+", " ", (content or "").strip())
    return c[:40]


def extract_tag_query(tags):
    """tags 诊断查询：剔除元数据/泛化标签，只保留可能有语义的标签。

    signal:* / occurred:* 是检索辅助元数据，auto_consolidated 与 pattern 是全库
    泛化标记；把它们当用户关键词会让目标被同质记忆淹没，且不代表现实查询。
    """
    kw = []
    for raw in parse_tags(tags):
        t = raw.strip()
        low = t.lower()
        if not t or low.startswith(("signal:", "occurred:")):
            continue
        if low in {"auto_consolidated", "pattern"}:
            continue
        kw.append(t)
    return " ".join(kw[:8])


def stratified_quotas(counts, n):
    """按各层活跃存量比例分配样本，使用最大余数法并处理小层/总量不足。"""
    total = sum(counts.values())
    n = min(max(0, n), total)
    if not total or not n:
        return {k: 0 for k in counts}
    quotas = {k: min(counts[k], int(n * counts[k] / total)) for k in counts}
    while sum(quotas.values()) < n:
        candidates = [k for k in counts if quotas[k] < counts[k]]
        if not candidates:
            break
        # 目标配额与实际配额差最大者优先；稳定 tie-break 便于测试复现。
        k = max(candidates,
                key=lambda x: (n * counts[x] / total - quotas[x], counts[x], x))
        quotas[k] += 1
    return quotas


def sample_memories(db, n):
    """按 tags 类型分层随机抽样当前活跃记忆，避免数据分布漂移改变指标含义。

    仍排除已 supersede / 已过期记忆；每层内部继续 ORDER BY RANDOM()，既保持
    生产抽样覆盖面，又避免 80 条样本中某层偶然过多造成 10+pp 的假回归。
    """
    con = sqlite3.connect(db, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    cur = con.cursor()
    active = """FROM memories
                WHERE namespace=? AND length(coalesce(content,''))>0
                  AND superseded_by IS NULL
                  AND (valid_to IS NULL OR valid_to = '' OR valid_to > datetime('now'))"""
    clauses = {
        "empty": "(tags IS NULL OR trim(tags) IN ('', '[]'))",
        "auto_consolidated": "(tags IS NOT NULL AND trim(tags) NOT IN ('', '[]') "
                             "AND tags LIKE '%auto_consolidated%')",
        "normal": "(tags IS NOT NULL AND trim(tags) NOT IN ('', '[]') "
                  "AND tags NOT LIKE '%auto_consolidated%')",
    }
    counts = {}
    for kind, clause in clauses.items():
        counts[kind] = cur.execute(
            f"SELECT COUNT(*) {active} AND {clause}", (NS,)).fetchone()[0]
    quotas = stratified_quotas(counts, n)
    rows = []
    for kind in ("empty", "auto_consolidated", "normal"):
        quota = quotas[kind]
        if not quota:
            continue
        rows.extend(cur.execute(
            f"SELECT id, content, tags {active} AND {clauses[kind]} "
            "ORDER BY RANDOM() LIMIT ?", (NS, quota)).fetchall())
    con.close()
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reset", action="store_true",
                    help="复位冷态（清零 access_count/last_recalled）再测；"
                         "默认不复位，以反映生产真实热态召回（与 92.5%% 基线口径一致）")
    ap.add_argument("--json-out", default=None,
                    help="明细 JSON 路径；默认每次自动保存 recall_guard_detail_时间戳.json")
    ap.add_argument("--sample-n", type=int, default=SAMPLE_N,
                    help=f"分层抽样条数（默认 {SAMPLE_N}）")
    args = ap.parse_args()

    admin_key = load_env(ENV_PATH).get("MEMORIA_ADMIN_KEY", "")
    rows = sample_memories(DB, args.sample_n)
    if not rows:
        print(f"[guard] FATAL: 命名空间 {NS} 无可用记忆（或库不可达 {DB}）")
        sys.exit(2)

    if args.reset:
        reset_cold(DB)

    n_full = n_content_kw = n_tag = 0
    n_attempt_full = 0
    hit_full = hit_content_kw = hit_tag = hit_full10 = 0
    detail = []
    layers = {"empty": 0, "auto_consolidated": 0, "normal": 0}
    t0 = time.time()
    for mid, content, tags in rows:
        if time.time() - t0 > GLOBAL_CAP:
            print(f"  [guard] 触达全局上限 {GLOBAL_CAP}s，提前结束"
                  f"（已测 {n_full} 条完整 / {n_content_kw} 条正文关键词 / {n_tag} 条tags）")
            break
        n_attempt_full += 1
        full_q = (content or "")[:4000]
        try:
            top = recall_topk(full_q, admin_key, K)
        except Exception as e:
            print("  full recall FAIL:", e)
            continue
        top_ids = [x.get("memory_id") for x in top]
        full_rank = top_ids.index(mid) + 1 if mid in top_ids else None
        n_full += 1
        hit_full += full_rank is not None and full_rank <= 5
        hit_full10 += full_rank is not None and full_rank <= 10

        # 正文关键词：唯一参与关键词告警的口径，所有分层使用同一构造规则。
        content_kw_q = extract_content_query(content)
        content_kw_rank = None
        if content_kw_q:
            try:
                top_kw = recall_topk(content_kw_q, admin_key, K)
                ids = [x.get("memory_id") for x in top_kw]
                content_kw_rank = ids.index(mid) + 1 if mid in ids else None
                n_content_kw += 1
                hit_content_kw += content_kw_rank is not None and content_kw_rank <= 5
            except Exception as e:
                print("  content keyword recall FAIL:", e)

        # tags 查询：独立诊断，不参与退出码/告警；元数据 tags 不构造查询。
        tag_q = extract_tag_query(tags)
        tag_rank = None
        if tag_q:
            try:
                top_tag = recall_topk(tag_q, admin_key, K)
                ids = [x.get("memory_id") for x in top_tag]
                tag_rank = ids.index(mid) + 1 if mid in ids else None
                n_tag += 1
                hit_tag += tag_rank is not None and tag_rank <= 5
            except Exception as e:
                print("  tag recall FAIL:", e)

        kind = tag_kind(tags)
        layers[kind] += 1
        detail.append({"id": mid, "tag_kind": kind,
                       "content_kw_query": content_kw_q, "tag_query": tag_q,
                       "full_rank": full_rank, "content_kw_rank": content_kw_rank,
                       "tag_rank": tag_rank})
        if n_full % 20 == 0:
            print(f"  ...{n_full} ({time.time()-t0:.0f}s)")

    full_r5 = hit_full / n_full if n_full else 0.0
    full_r10 = hit_full10 / n_full if n_full else 0.0
    content_kw_r5 = hit_content_kw / n_content_kw if n_content_kw else 0.0
    tag_r5 = hit_tag / n_tag if n_tag else 0.0

    print("\n================ 召回回归守护（分层抽样 @5） ================")
    print(f"抽样命名空间 : {NS}")
    print(f"抽样分层     : 空tags {layers['empty']} / 自动整合 {layers['auto_consolidated']} / 普通tags {layers['normal']}")
    print(f"样本数       : 完整内容 {n_full} / 正文关键词 {n_content_kw} / tags诊断 {n_tag}")
    print(f"完整内容@5   : {full_r5*100:.1f}%   (基线 {FULL_BASELINE*100:.1f}%, 报警阈值 {FULL_WARN*100:.0f}%)")
    print(f"完整内容@10  : {full_r10*100:.1f}%   (仅诊断用)")
    print(f"正文关键词@5 : {content_kw_r5*100:.1f}%   (基线 {CONTENT_KW_BASELINE*100:.1f}%, 报警阈值 {CONTENT_KW_WARN*100:.0f}%)")
    tag_text = f"{tag_r5*100:.1f}%" if n_tag else "N/A"
    print(f"tags查询@5   : {tag_text}   (仅诊断，不参与告警)")

    if full_r5 < FULL_WARN:
        status = "RECALL_DEGRADED"
    elif content_kw_r5 < CONTENT_KW_WARN:
        status = "SEMANTIC_DEGRADED"
    else:
        status = "OK"
    if n_full == 0:
        print(f"\n❌ GUARD ERROR：命名空间 {NS} 共 {n_attempt_full} 次 full recall 调用全部失败，"
              f"无法连接 memoria（{MCP_URL}）或查询超时——判定为运行错误（exit 2），"
              f"已跳过趋势写入以避免以 0% 污染历史")
        sys.exit(2)
    if n_content_kw == 0:
        print(f"\n❌ GUARD ERROR：完整内容查询成功，但 {n_full} 次正文关键词 recall 全部失败/为空，"
              f"无法计算 content_keyword@5——判定为运行错误（exit 2），已跳过趋势写入")
        sys.exit(2)

    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    detail_path = (args.json_out or os.environ.get("RECALL_DETAIL_JSON") or
                   os.path.join(HERE, f"recall_guard_detail_{time.strftime('%Y%m%d_%H%M%S')}.json"))
    try:
        with open(detail_path, "w", encoding="utf-8") as f:
            json.dump(detail, f, ensure_ascii=False, indent=2)
        print(f"明细 -> {detail_path}")
    except Exception as e:
        print(f"[guard] 明细写入失败: {e}")

    trend_line = (f"{ts},{full_r5:.4f},{content_kw_r5:.4f},{tag_r5:.4f},"
                  f"{n_full},{n_content_kw},{n_tag},{status}\n")
    try:
        append_trend(TREND, trend_line)
        print(f"趋势已追加 -> {TREND}")
    except Exception as e:
        print(f"[guard] 趋势写入失败: {e}")

    consec = count_consecutive_degraded(TREND)
    severity = "ok" if status == "OK" else ("elevated" if consec >= CONSEC_WARN else "warn")
    alert = {
        "timestamp": ts,
        "status": status,
        "severity": severity,
        "consecutive_degraded": consec,
        "full_r5": round(full_r5, 4),
        "full_r10": round(full_r10, 4),
        # kw_* 保留给既有 memory_ops_status/巡检消费者；语义已明确为正文关键词。
        "kw_r5": round(content_kw_r5, 4),
        "content_kw_r5": round(content_kw_r5, 4),
        "tag_r5": round(tag_r5, 4),
        "n_full": n_full,
        "n_kw": n_content_kw,
        "n_content_kw": n_content_kw,
        "n_tag": n_tag,
        "sample_layers": layers,
        "detail_path": detail_path,
        "thresholds": {
            "full_warn": FULL_WARN,
            "kw_warn": CONTENT_KW_WARN,
            "content_kw_warn": CONTENT_KW_WARN,
            "full_baseline": FULL_BASELINE,
            "kw_baseline": CONTENT_KW_BASELINE,
            "content_kw_baseline": CONTENT_KW_BASELINE,
            "consec_warn": CONSEC_WARN,
        },
        "namespace": NS,
        "exit_hint": 0 if status == "OK" else 1,
    }
    try:
        with open(ALERT, "w", encoding="utf-8") as f:
            json.dump(alert, f, ensure_ascii=False, indent=2)
        print(f"告警快照 -> {ALERT} (severity={severity}, consec={consec})")
    except Exception as e:
        print(f"[guard] 告警写入失败: {e}")

    if status == "OK":
        print("\n✅ 召回达标（完整内容@5 与 正文关键词@5 均正常；tags查询仅诊断）")
        sys.exit(0)
    elif status == "RECALL_DEGRADED":
        print(f"\n⚠️ 召回检索劣化：完整内容@5={full_r5*100:.1f}% 低于阈值 "
              f"{FULL_WARN*100:.0f}%（基线 {FULL_BASELINE*100:.1f}%）—— "
              f"embedding / 索引真故障，记忆无法被自身内容召回")
    else:
        print(f"\n⚠️ 正文关键词召回劣化：content_keyword@5={content_kw_r5*100:.1f}% 低于阈值 "
              f"{CONTENT_KW_WARN*100:.0f}%（基线 {CONTENT_KW_BASELINE*100:.1f}%）—— "
              f"正文短查询召回下降，完整内容@5={full_r5*100:.1f}%")
    if consec >= CONSEC_WARN:
        print(f"   🔴 连续 {consec} 次劣化，已达 elevated——建议核对最近部署/索引变更")
    sys.exit(1)


def count_consecutive_degraded(trend_path):
    """从趋势 CSV 尾部统计连续非 OK 次数（含本次）。文件缺失/空返回 0。"""
    try:
        with open(trend_path, "r", encoding="utf-8") as f:
            lines = [ln.strip() for ln in f if ln.strip()]
    except OSError:
        return 0
    if not lines:
        return 0
    # 跳过 header
    if lines[0].startswith("timestamp"):
        lines = lines[1:]
    n = 0
    for ln in reversed(lines):
        parts = ln.split(",")
        if len(parts) < 8:
            break
        status = parts[-1]
        if status == "OK":
            break
        n += 1
    return n


if __name__ == "__main__":
    main()
