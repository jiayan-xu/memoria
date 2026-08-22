#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""LongMemEval 完整集回归（500 问）—— memoria 全链路评测

流程（对齐 LoCoMo 评测方法论，见 eval/locomo/locomo_eval.py）：
1. 灌库：每问一个命名空间 longmemeval/<question_id>，其 haystack 会话按
   LME_CHUNK_CHARS 分块，每块一条 memory_remember（content 带 [date] [session_id]
   头，raw_ref 占位绕开 distill 压缩——评测口径为「原文召回」，压缩是独立变量，
   见 HY3 执行单 §7 / H4：回归指标，非产品 KPI）。
2. 检索：memory_search_v2（prod 配置：recall_depth=100 + cross-encoder rerank pool=100）。
3. 证据命中：答案是否来自 answer_session_ids 所在块（从 content 头解析 session id）——
   隔离「检索」与「LLM 作答」两个环节。
4. 答案生成：检索上下文 + question → DeepSeek deepseek-v4-flash（官方 api.deepseek.com，同 LoCoMo 口径）。
5. Judge：0-10 宽松打分（同 LoCoMo judge prompt），按 question_type / 能力域聚合。

用法（数据文件放脚本同目录，或 LME_DATA_PATH 指定）：
  python longmemeval_eval.py [--max-questions N] [--limit-questions N] [--skip-ingest]
                              [--dry-run] [--ingest-workers W]
env:
  LME_DATA_PATH / LME_CHUNK_CHARS(5000) / LME_TOP_K(15) / LME_CTX_CHARS(1200)
  LME_CHAT_MODEL / LME_JUDGE_MODEL / LME_BATCH_WAIT(0.05) / LME_INGEST_WORKERS(4)
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.request
import urllib.error
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

sys.stdout.reconfigure(encoding="utf-8")

MEMORIA_URL = "http://127.0.0.1:9003/mcp"
AGENT_ID = "jarvis"
AGENT_KEY = os.environ.get("MEMORIA_JARVIS_BADGE", "")
NS_PREFIX = "longmemeval"

CHUNK_CHARS = int(os.environ.get("LME_CHUNK_CHARS", "5000"))
TOP_K = int(os.environ.get("LME_TOP_K", "8"))
CTX_CHARS = int(os.environ.get("LME_CTX_CHARS", "2000"))  # 检索响应内截断上限（服务端 2000）
BATCH_WAIT = float(os.environ.get("LME_BATCH_WAIT", "0.05"))
INGEST_WORKERS = int(os.environ.get("LME_INGEST_WORKERS", "4"))
CHAT_MODEL = os.environ.get("LME_CHAT_MODEL", "deepseek-v4-flash")
JUDGE_MODEL = os.environ.get("LME_JUDGE_MODEL", "deepseek-v4-flash")

HERE = os.path.dirname(os.path.abspath(__file__))
DATA_PATH = os.environ.get(
    "LME_DATA_PATH",
    os.path.join(HERE, "longmemeval_s_cleaned.json"),
)
RESULTS_PATH = os.path.join(HERE, "longmemeval_results.json")

# ── DeepSeek 官方 API key：优先环境变量，兜底读 ~/agent-core/.env（不写死绝对路径）──
DS_KEY = os.environ.get("DEEPSEEK_API_KEY") or os.environ.get("AGENT_API_KEY") or ""
if not DS_KEY:
    cand = os.path.join(os.path.expanduser("~"), "agent-core", ".env")
    if os.path.exists(cand):
        for line in open(cand, encoding="utf-8"):
            if line.startswith("DEEPSEEK_API_KEY="):
                DS_KEY = line.strip().split("=", 1)[1].strip().strip('"').strip("'")
                break
        if not DS_KEY:
            for line in open(cand, encoding="utf-8"):
                if line.startswith("AGENT_API_KEY="):
                    DS_KEY = line.strip().split("=", 1)[1].strip().strip('"').strip("'")
                    break
if not AGENT_KEY:
    cand = os.path.join(os.path.expanduser("~"), "agent-core", ".env")
    if os.path.exists(cand):
        for line in open(cand, encoding="utf-8"):
            if line.startswith("MEMORIA_JARVIS_BADGE="):
                AGENT_KEY = line.strip().split("=", 1)[1].strip().strip('"').strip("'")
                break

CHAT_URL = "https://api.deepseek.com/v1/chat/completions"

# question_type → 论文 5 能力域（cleaned 数据无 capability 字段，按官方任务命名映射，
# 报告里标注为近似映射）
CAPABILITY = {
    "single-session-user": "Information Extraction",
    "single-session-assistant": "Information Extraction",
    "multi-session": "Multi-Session Reasoning",
    "knowledge-update": "Knowledge Updates",
    "temporal-reasoning": "Temporal Reasoning",
    "single-session-preference": "Abstraction & Reasoning",
}


def mcp_call(tool: str, arguments: dict, timeout: int = 60, retries: int = 4) -> dict:
    """MCP 调用（带重试：服务器被 watchdog 重启/瞬断时等待恢复）"""
    last = None
    for attempt in range(retries):
        try:
            body = json.dumps({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }).encode("utf-8")
            req = urllib.request.Request(
                MEMORIA_URL, data=body,
                headers={"Content-Type": "application/json",
                         "X-Agent-Id": AGENT_ID, "X-Agent-Key": AGENT_KEY})
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            if "error" in data:
                raise RuntimeError(f"MCP error: {data['error']}")
            return json.loads(data["result"]["content"][0]["text"])
        except Exception as e:
            last = e
            time.sleep(min(2 ** attempt, 8))  # 1,2,4,8s
    raise last


def remember_chunk(content: str, ns: str, sid: str, qid: str, date: str, part: int) -> None:
    """灌一块（raw_ref 占位绕开 distill；tags 携带 session/occurred 供诊断）"""
    mcp_call("memory_remember", {
        "content": content,
        "namespace": ns,
        "category": "fact",
        "importance": 3,
        "source": "longmemeval",
        "raw_ref": f"longmemeval:{qid}:{sid}:part{part}",
        "tags": json.dumps([
            f"longmemeval:{qid}",
            f"session:{sid}",
            f"occurred:{str(date)[:10]}",
        ]),
    }, timeout=180)


def chunk_text(text: str, size: int) -> list:
    """按行聚合切块，超长单行硬切；尽量在换行附近断开"""
    if len(text) <= size:
        return [text]
    chunks = []
    cur = ""
    for line in text.split("\n"):
        if len(cur) + len(line) + 1 > size:
            if cur:
                chunks.append(cur)
                cur = ""
            # 单行仍超长：硬切
            while len(line) > size:
                chunks.append(line[:size])
                line = line[size:]
        cur = (cur + "\n" + line).strip() if cur else line.strip()
    if cur:
        chunks.append(cur)
    return chunks


def ingest_question(item) -> dict:
    qid = item["question_id"]
    ns = f"{NS_PREFIX}/{qid}"
    sid_list = item["haystack_session_ids"]
    date_list = item["haystack_dates"]
    sessions = item["haystack_sessions"]
    jobs = []
    for sid, date, sess in zip(sid_list, date_list, sessions):
        turns = "\n".join(
            f"{m.get('role', '?')}: {m.get('content', '')}" for m in sess)
        text = f"[{date}] [{sid}]\n{turns}"
        parts = chunk_text(text, CHUNK_CHARS)
        n = len(parts)
        # 每个分块都带 [date] [sid] 头（含 part 序号）——续块也可自证归属，
        # answer_hit 的 session 解析依赖它（实测：仅首块带头时续块 sid=None）
        for i, body in enumerate(parts):
            header = f"[{date}] [{sid}] (part {i + 1}/{n})\n"
            jobs.append((header + body, ns, sid, qid, date, i))
    fails = 0

    def _one(job):
        nonlocal fails
        try:
            remember_chunk(*job)
        except Exception as e:
            fails += 1
            print(f"  ingest fail {qid} {job[2]} part{job[5]}: {e}")

    with ThreadPoolExecutor(max_workers=INGEST_WORKERS) as ex:
        list(ex.map(_one, jobs))
        time.sleep(BATCH_WAIT)
    return {"chunks": len(jobs), "fails": fails}


def search(query: str, ns: str) -> list:
    # 搜索单次尝试 + 短超时：慢查询不重试放大服务端压力（失败由收尾重试兜底）
    r = mcp_call("memory_search_v2", {
        "query": query, "namespace": ns, "max_results": TOP_K,
    }, timeout=25, retries=1)
    return r.get("results", [])


def chat(messages: list, model: str = None, max_tokens: int = 512,
         temperature: float = 0.2) -> str:
    if not DS_KEY:
        return "[ERR:NO_KEY]"
    body = json.dumps({
        "model": model or CHAT_MODEL, "messages": messages,
        "max_tokens": max_tokens, "temperature": temperature,
    }).encode("utf-8")
    req = urllib.request.Request(
        CHAT_URL, data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {DS_KEY}"})
    for attempt in range(2):
        try:
            with urllib.request.urlopen(req, timeout=90) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            return data["choices"][0]["message"]["content"].strip()
        except Exception as e:
            if attempt == 1:
                return f"[ERR:{e}]"
            time.sleep(3 * (attempt + 1))
    return "[ERR]"


def judge(question: str, gold: str, pred: str):
    prompt = (
        "You are evaluating an AI assistant's answer to a question about a long conversation.\n"
        "Score the predicted answer against the gold answer on a 0-10 scale.\n"
        "10 = fully correct. 8-9 = correct with minor imprecision (e.g. date off by a day, \n"
        "or slightly different wording). 5-7 = partially correct / missing detail. \n"
        "0-4 = wrong, hallucinated, or UNKNOWN when answer was available.\n"
        "Be LENIENT: near-miss answers (right entity, right event, slightly wrong date) \n"
        "should score 8+. Only exact factual errors drop below 5.\n"
        f"Question: {question}\n"
        f"Gold answer: {gold}\n"
        f"Predicted answer: {pred}\n"
        "Reply with ONLY a number between 0 and 10."
    )
    out = chat([{"role": "user", "content": prompt}], model=JUDGE_MODEL,
               max_tokens=10, temperature=0.0)
    if out.startswith("[ERR"):
        return None
    m = re.search(r"(\d+(?:\.\d+)?)\s*(?:/10)?", out)
    if m:
        v = float(m.group(1))
        if v > 10:
            v = v / 10.0
        return max(0.0, min(10.0, v))
    return None


def fetch_memory(memory_id: str) -> str:
    """经 Web API 取完整记忆内容（search 响应被服务端截断 2000 字符，答案句可能在块尾部）"""
    req = urllib.request.Request(
        f"http://127.0.0.1:9003/api/memories?id={urllib.parse.quote(memory_id)}",
        headers={"X-Agent-Id": AGENT_ID, "X-Agent-Key": AGENT_KEY})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    return data.get("content", "")


def session_of(content: str):
    r"""从 content 头解析 session id：'[date] [sid] (part k/n)...'——日期含空格，
    首段必须用 [^\]]+ 而非 \S+"""
    m = re.match(r"^\[[^\]]+\] \[([^\]]+)\]", content)
    return m.group(1) if m else None


def run_qa(item) -> dict:
    qid = item["question_id"]
    ns = f"{NS_PREFIX}/{qid}"
    q = item.get("question", "")
    gold = item.get("answer", "")
    qtype = item.get("question_type", "?")
    ans_sids = set(item.get("answer_session_ids", []))
    base = {
        "question_id": qid, "question_type": qtype,
        "capability": CAPABILITY.get(qtype, "Unknown"),
        "question": q, "gold": str(gold),
    }
    try:
        results = search(q, ns)
        retrieved = len(results)
        hit_sids = set()
        for r in results:
            sid = session_of(r.get("content", ""))
            if sid:
                hit_sids.add(sid)
        answer_hit = bool(ans_sids & hit_sids)
        # 取完整块内容（服务端 search 响应截断 2000 字符；答案句常在块中后部）
        ctx_lines = []
        for i, r in enumerate(results[:TOP_K]):
            c = r.get("content", "")[:CTX_CHARS]
            try:
                full = fetch_memory(r["memory_id"])
                if full:
                    c = full
            except Exception:
                pass  # 拉取失败用截断内容兜底
            ctx_lines.append(f"[{i+1}] {c}")
        ctx = "\n".join(ctx_lines) if ctx_lines else "(no retrieved memories)"
        prompt = (
            "You are answering questions about a user's long-term conversation history.\n"
            "Use ONLY the retrieved conversation excerpts below to answer. "
            "If the answer is not in the excerpts, say 'UNKNOWN'.\n"
            f"Retrieved excerpts:\n{ctx}\n\n"
            f"Question: {q}\n"
            "Answer concisely (1-3 sentences or a short phrase):"
        )
        pred = chat([{"role": "user", "content": prompt}])
        if pred.startswith("[ERR"):
            return None  # 生成失败：不落盘，留给 resume 重试
        score = judge(q, str(gold), pred)
        if score is None:
            return None  # judge 失败：同上
    except Exception as e:
        return None  # 检索/网络异常：不落盘，留给 resume 重试
    return {**base, "pred": pred, "score": score, "answer_hit": answer_hit,
            "retrieved": retrieved}


def load_done():
    if os.path.exists(RESULTS_PATH):
        with open(RESULTS_PATH, encoding="utf-8") as f:
            return json.load(f)
    return []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--max-questions", type=int, default=0, help="0=全部 500")
    ap.add_argument("--limit-questions", type=int, default=0, help="截取前 N 问（调试）")
    ap.add_argument("--skip-ingest", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--ingest-workers", type=int, default=INGEST_WORKERS)
    args = ap.parse_args()

    if not AGENT_KEY:
        print("FATAL: MEMORIA_JARVIS_BADGE 未设置")
        sys.exit(1)
    if not os.path.exists(DATA_PATH):
        print(f"FATAL: 数据文件不存在 {DATA_PATH}（下载到脚本同目录或设 LME_DATA_PATH）")
        sys.exit(1)
    if not DS_KEY:
        print("WARN: DEEPSEEK_API_KEY 未找到")

    with open(DATA_PATH, encoding="utf-8") as f:
        data = json.load(f)
    if args.limit_questions > 0:
        data = data[: args.limit_questions]
    if args.max_questions > 0:
        data = data[: args.max_questions]
    print(f"== LongMemEval Eval: {len(data)} questions (chunk={CHUNK_CHARS}, top_k={TOP_K}, workers={args.ingest_workers}) ==")

    done = load_done()
    done_ids = {d["question_id"] for d in done}
    results = list(done)

    t0 = time.time()
    for i, item in enumerate(data):
        qid = item["question_id"]
        if qid in done_ids:
            print(f"[{i+1}/{len(data)}] {qid}: resume 跳过（已有结果）")
            continue
        if args.dry_run:
            n_sess = len(item["haystack_session_ids"])
            n_chars = sum(len(m.get("content", "")) for s in item["haystack_sessions"] for m in s)
            print(f"[{i+1}/{len(data)}] {qid} {item['question_type']}: "
                  f"sessions={n_sess} chars={n_chars} chunks≈{n_chars // CHUNK_CHARS + 1}")
            continue
        if not args.skip_ingest:
            ingest = ingest_question(item)
            n = ingest["chunks"]
            if ingest["fails"]:
                print(f"  WARN: {qid} 灌库失败 {ingest['fails']}/{n} 块（已容错继续）")
        else:
            n = 0
        res = run_qa(item)
        if res is None:
            # 失败不落盘：下次 resume 重试（服务端重启窗口内不丢题）
            print(f"[{i+1}/{len(data)}] {qid}: 本轮失败（不落盘，resume 重试）")
            continue
        res["chunks"] = n
        results.append(res)
        with open(RESULTS_PATH, "w", encoding="utf-8") as f:
            json.dump(results, f, ensure_ascii=False, indent=1)
        s = res["score"]
        print(f"[{i+1}/{len(data)}] {qid} {item['question_type']} "
              f"score={s if s is None else round(s, 1)} hit={res['answer_hit']} "
              f"retrieved={res['retrieved']} chunks={n} ({time.time()-t0:.0f}s elapsed)")

    # 收尾重试：历史 score=None 条目（重启窗口/LLM 抖动造成的）再尝试至多 2 轮
    for attempt in range(2):
        none_ids = [r["question_id"] for r in results if r.get("score") is None]
        if not none_ids:
            break
        for item in data:
            if item["question_id"] not in none_ids:
                continue
            retry = run_qa(item)
            if retry is not None:
                results = [r for r in results if r["question_id"] != item["question_id"]]
                results.append(retry)
                with open(RESULTS_PATH, "w", encoding="utf-8") as f:
                    json.dump(results, f, ensure_ascii=False, indent=1)
                print(f"retry {item['question_id']}: score={retry['score']} hit={retry['answer_hit']}")
            else:
                print(f"retry {item['question_id']}: 仍失败（第 {attempt+1} 轮）")
        # 重新加载最新文件状态（避免与磁盘不一致）
        results = load_done()

    scored = [r for r in results if r["score"] is not None]
    if not scored:
        print("无有效评分（dry-run 或全部跳过）")
        return
    avg = sum(r["score"] for r in scored) / len(scored)
    hit = [r for r in results if r.get("answer_hit")]
    print("\n== 结果 ==")
    print(f"总 QA: {len(results)}（评分 {len(scored)}，跳过 {len(results)-len(scored)}）  平均分: {avg:.2f}")
    print(f"证据命中率（answer session 进 top-{TOP_K}）: {len(hit)}/{len(results)} = {100*len(hit)/max(len(results),1):.1f}%")
    print(f"pass@8 占比: {100*sum(1 for r in scored if r['score'] >= 8)/len(scored):.1f}%")
    print("按 question_type:")
    by_type = {}
    for r in results:
        by_type.setdefault(r["question_type"], []).append(r)
    for qt, rs in sorted(by_type.items()):
        ss = [r["score"] for r in rs if r["score"] is not None]
        hs = sum(1 for r in rs if r.get("answer_hit"))
        if ss:
            print(f"  {qt}: avg={sum(ss)/len(ss):.2f} (n={len(ss)}/{len(rs)}) hit={100*hs/max(len(rs),1):.0f}%")
    print("按能力域:")
    by_cap = {}
    for r in results:
        by_cap.setdefault(r["capability"], []).append(r)
    for cap, rs in sorted(by_cap.items()):
        ss = [r["score"] for r in rs if r["score"] is not None]
        if ss:
            print(f"  {cap}: avg={sum(ss)/len(ss):.2f} (n={len(ss)}/{len(rs)})")
    unknown = [r for r in results if str(r.get("pred", "")).upper() == "UNKNOWN"]
    print(f"UNKNOWN 回答: {len(unknown)}")
    print(f"结果已保存: {RESULTS_PATH}")


if __name__ == "__main__":
    main()
