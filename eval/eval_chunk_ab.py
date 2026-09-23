#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
P2 A/B：heading-aligned 切块 vs fixed-size 切块
==============================================
问题：文档层切块「按 Markdown 标题边界」是否比「固定字符切」更利于检索（recall@k）
      以及是否减少证据被撕开（integrity）。

方法（离线、无网络、无 embed）：
  1. 语料 = 合成多章节 MD（可控真值）+ 仓库 docs/*.md 真实章节（附真值句）
  2. 三种切块器（与 src/document.rs 同算法的 Python 镜像）：
       A fixed-512 / B fixed-3500 / C heading-aligned（超长段退回字符/表格行切）
  3. 检索 = 字符 2-gram TF-IDF 余弦（对中文稳定，零依赖）
  4. 指标：
       recall@1/3/5  — 查询命中的 top-k 块是否包含金句
       intact_rate   — 金句完整落在「某一个」块内的比例（防撕开）
       avg_chunks    — 切块数量（效率代理）

用法：
  python eval/eval_chunk_ab.py
  python eval/eval_chunk_ab.py --json eval/chunk_ab_result.json
"""
from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))

CHUNK_OVERLAP = 200  # 对齐 document.rs::chunk_text


# ── 切块器（镜像 src/document.rs）────────────────────────────────

def chunk_text(text: str, chunk_chars: int) -> list[str]:
    chars = list(text)
    if not chars:
        return []
    out = []
    i = 0
    while i < len(chars):
        end = min(i + chunk_chars, len(chars))
        out.append("".join(chars[i:end]))
        if end >= len(chars):
            break
        i = max(end - CHUNK_OVERLAP, i + 1)
    return out


def parse_atx_heading(line: str):
    t = line.rstrip()
    if not t.startswith("#"):
        return None
    level = 0
    for c in t:
        if c == "#":
            level += 1
        else:
            break
    if level == 0 or level > 4:
        return None
    rest = t[level:].strip()
    if not rest:
        return None
    title = rest.rstrip("#").strip()
    return (level, title) if title else None


def split_markdown_sections(text: str):
    sections = []
    stack = []
    current = []
    in_fence = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            current.append(line)
            continue
        if not in_fence:
            h = parse_atx_heading(line)
            if h:
                path = [t for _, t in stack]
                if "".join(current).strip():
                    sections.append((path, "\n".join(current) + "\n"))
                current = []
                level, title = h
                while stack and stack[-1][0] >= level:
                    stack.pop()
                stack.append((level, title))
                current.append(line)
                continue
        current.append(line)
    path = [t for _, t in stack]
    if "".join(current).strip():
        sections.append((path, "\n".join(current) + "\n"))
    return sections


def is_table_row(line: str) -> bool:
    return "\t" in line or (line.count("|") >= 2)


def chunk_preserving_rows(body: str, chunk_chars: int) -> list[str]:
    lines = body.splitlines()
    if not lines:
        return []
    table_rows = sum(1 for l in lines if is_table_row(l))
    if table_rows * 2 < len(lines):
        return chunk_text(body, chunk_chars)
    header = next((l for l in lines if is_table_row(l)), lines[0])
    header_line = header + "\n"
    out = []
    current = ""
    for line in lines:
        if line == header:
            continue
        line_len = len(line) + 1
        base = len(header_line) if not current else len(current)
        if base + line_len > chunk_chars and current:
            out.append(current)
            current = header_line
        if not current:
            current = header_line
        current += line + "\n"
    if current.strip():
        out.append(current)
    return out or [body]


def chunk_heading_aligned(text: str, chunk_chars: int = 3500) -> list[str]:
    sections = split_markdown_sections(text)
    if not sections:
        return []
    if len(sections) == 1 and not sections[0][0]:
        return chunk_text(text, chunk_chars)
    out = []
    for _path, body in sections:
        if len(body) <= chunk_chars:
            out.append(body)
        else:
            out.extend(chunk_preserving_rows(body, chunk_chars))
    return out


# ── 检索：字符 2-gram TF-IDF ─────────────────────────────────────

def ngrams(s: str, n: int = 2) -> Counter:
    s = re.sub(r"\s+", " ", s.lower())
    if len(s) < n:
        return Counter([s]) if s else Counter()
    return Counter(s[i : i + n] for i in range(len(s) - n + 1))


def tfidf_rank(query: str, docs: list[str], k: int = 5) -> list[int]:
    """返回 top-k 文档下标（按余弦相似度降序）。"""
    q = ngrams(query)
    if not q:
        return list(range(min(k, len(docs))))
    dcs = [ngrams(d) for d in docs]
    N = len(dcs)
    df = Counter()
    for dc in dcs:
        for t in dc:
            df[t] += 1
    def vec(c: Counter) -> dict:
        return {t: (1 + math.log(tf)) * math.log((N + 1) / (df[t] + 0.5) + 1) for t, tf in c.items()}
    qv = {t: (1 + math.log(tf)) * math.log((N + 1) / (df.get(t, 0) + 0.5) + 1) for t, tf in q.items()}
    qn = math.sqrt(sum(x * x for x in qv.values())) or 1.0
    scores = []
    for i, dc in enumerate(dcs):
        dv = vec(dc)
        if not dv:
            scores.append((0.0, i))
            continue
        dn = math.sqrt(sum(x * x for x in dv.values())) or 1.0
        dot = sum(qv[t] * dv.get(t, 0.0) for t in qv)
        scores.append((dot / (qn * dn), i))
    scores.sort(key=lambda x: (-x[0], x[1]))
    return [i for _, i in scores[:k]]


# ── 语料 + 金标 ─────────────────────────────────────────────────

def synthetic_docs() -> list[dict]:
    """可控真值：长金段（易被 fixed 撕开）+ 标题导向查询 + 超长干扰前言。"""
    topics = [
        ("固废考核", [
            ("卸料扬尘判定",
             "卸料扬尘以垃圾吊坑内 F0 与 F3 帧为准。判定流程第一步固定镜头核对坑底堆积高度，"
             "第二步对比卸料前静止帧与抓斗翻动瞬间的灰度方差，第三步排除门前过车造成的运动模糊拖影。"
             "仅当坑内灰度方差超过阈值且持续三帧以上时记为强扬尘。"),
            ("取样抽查",
             "取样抽查每车留大样 10.1 公斤与小样 1.0 公斤。台账编号按 202608-NN 连续编号不得跳号。"
             "Word 记录表文件名必须与台账编号一致，DB 的 serial_no 与 output_file 三层对齐。"
             "漏记时优先核对 detector 的 sample_state 先标记后落库缺陷。"),
            ("归档规则",
             "进厂时间 HH 时 MM 分 等于一车一趟。照片与录像扁平放在根目录，不要建立 1/2 子夹。"
             "若出现进厂时间 X/1 与 /2，表示当日第 N 车，按进厂时间升序对号入座。"
             "企业级理文的 /1 桶规则不变，其余走嵌套趟次归位。"),
        ]),
        ("NVR 录像下载", [
            ("下载策略",
             "白名单车辆一律下载全机位五路，准确优先于省流量。workers 默认为 2，"
             "避免挤占单通道实时预览导致 cam 黑屏。夜间计划任务每日 21:00 触发，"
             "完成后再跑校验空镜与归档后处理。"),
            ("命名约定",
             "录像文件名格式为 车牌_通道中文名_进厂出厂时刻.mp4。"
             "示例 苏EBD570_垃圾吊控制室_0904-0908.mp4。通道名使用中文业务名而非 cam_id。"
             "事件片另加 _event 后缀并与源视频同目录存放。"),
        ]),
        ("Memoria 记忆系统", [
            ("双层契约",
             "文档层使用干净 Markdown 分块并挂清单 parent。记忆层使用原子事实，"
             "通过 parent_id 与 raw_ref 挂回文档。原件只进 raw_ref 不进 content。"
             "禁止用 LLM 反复清洗抽取文本，以免把正确内容改坏。"),
            ("标题切块",
             "chunk_text_structured 按井号一到四级标题边界切分。超长表格段按行切并重复表头，"
             "避免字符切把 TSV 行撕成两截。分块头注入 文件名 与标题路径面包屑。"
             "无标题长文才退回固定字符切块。"
             "这里再补一段超长金句用于完整性对照：实现细节上 split_markdown_sections 维护标题栈，"
             "每段 body 含自身标题行与正文；代码围栏内的井号不当标题；"
             "chunk_preserving_rows 在表格行占比过半时按行累积，超出 chunk_chars 则另起一块并重复表头；"
             "ingest 侧 write_chunk_memories 统一写入 [文档块 i/N] 文件名 · 标题路径。"
             "若固定 512 字符切开这段，金句完整性会下降；标题对齐应保持整段完整。"
             "最后再补一些填充句以确保长度超过五百一十二字符。填充填充填充。"),
        ]),
    ]
    noise = ("流水日志占位：" + "".join(f"step-{i} ok; " for i in range(80))) * 3
    docs = []
    for di, (title, sections) in enumerate(topics):
        parts = [f"# {title} 操作手册\n\n引言：{noise}\n"]
        golds = []
        for st, para in sections:
            parts.append(f"\n## {st}\n\n背景说明：{noise[:200]}\n\n{para}\n\n补充：执行记录需写入当日台账。\n")
            golds.append({
                "gold_sentence": para,
                "section": st,
                "query": f"{title} {st} 的完整规则和流程",
            })
        parts.append("\n## 附录参数表\n\n列A\t列B\t列C\n")
        for i in range(150):
            parts.append(f"参数{i}\t值{i}\t{2023 + i % 5}\n")
        docs.append({"doc_id": f"synth-{di}-{title}", "text": "".join(parts), "golds": golds})
    return docs


def real_docs_from_repo() -> list[dict]:
    """从 docs/*.md 抽 1–2 个带标题的节作外部效度（金句=节内首句）。"""
    docs = []
    root = os.path.join(REPO, "docs")
    if not os.path.isdir(root):
        return docs
    for fn in sorted(os.listdir(root)):
        if not fn.endswith(".md"):
            continue
        path = os.path.join(root, fn)
        try:
            text = open(path, encoding="utf-8").read()
        except OSError:
            continue
        sections = split_markdown_sections(text)
        golds = []
        for path_t, body in sections:
            if not path_t or len(body) < 80:
                continue
            # 取非标题行的首个完整句
            for line in body.splitlines():
                s = line.strip()
                if not s or s.startswith("#") or s.startswith("|") or s.startswith("```"):
                    continue
                if len(s) >= 20:
                    golds.append({
                        "gold_sentence": s[:120],
                        "section": path_t[-1],
                        "query": (path_t[-1] + " 相关内容？")[:40],
                    })
                    break
            if len(golds) >= 3:
                break
        if golds and len(text) > 200:
            docs.append({"doc_id": f"real-{fn}", "text": text, "golds": golds})
        if len(docs) >= 3:
            break
    return docs


# ── 评测 ─────────────────────────────────────────────────────────

def evaluate(name: str, chunker, docs: list[dict]) -> dict:
    rec = {1: 0, 3: 0, 5: 0}
    intact = 0
    total = 0
    n_chunks = []
    details = []
    for doc in docs:
        chunks = chunker(doc["text"])
        n_chunks.append(len(chunks))
        for g in doc["golds"]:
            total += 1
            gold = g["gold_sentence"]
            # intact：金句完整落在某一块
            if any(gold in c for c in chunks):
                intact += 1
            ranked = tfidf_rank(g["query"], chunks, k=5)
            hit_at = None
            for k in (1, 3, 5):
                if hit_at is None and any(gold in chunks[i] for i in ranked[:k]):
                    hit_at = k
            if hit_at:
                for k in (1, 3, 5):
                    if hit_at <= k:
                        rec[k] += 1
            else:
                hit_at = 0
            details.append({
                "doc": doc["doc_id"],
                "section": g["section"],
                "hit_at": hit_at,
                "intact": gold in "".join(chunks) and any(gold in c for c in chunks),
            })
    return {
        "strategy": name,
        "n_queries": total,
        "recall@1": round(rec[1] / total, 4) if total else 0,
        "recall@3": round(rec[3] / total, 4) if total else 0,
        "recall@5": round(rec[5] / total, 4) if total else 0,
        "intact_rate": round(intact / total, 4) if total else 0,
        "avg_chunks": round(sum(n_chunks) / len(n_chunks), 2) if n_chunks else 0,
        "details": details,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", default=os.path.join(HERE, "chunk_ab_result.json"))
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()
    random.seed(args.seed)

    docs = synthetic_docs() + real_docs_from_repo()
    n_q = sum(len(d["golds"]) for d in docs)
    print(f"语料: {len(docs)} 文档 / {n_q} 查询\n")

    strategies = [
        ("fixed-512", lambda t: chunk_text(t, 512)),
        ("fixed-3500", lambda t: chunk_text(t, 3500)),
        ("heading-aligned", lambda t: chunk_heading_aligned(t, 3500)),
    ]
    results = []
    print(f"{'strategy':<18} {'R@1':>6} {'R@3':>6} {'R@5':>6} {'intact':>8} {'chunks':>8}")
    for name, fn in strategies:
        r = evaluate(name, fn, docs)
        results.append(r)
        print(
            f"{r['strategy']:<18} {r['recall@1']:>6} {r['recall@3']:>6} {r['recall@5']:>6} "
            f"{r['intact_rate']:>8} {r['avg_chunks']:>8}"
        )

    payload = {
        "note": "chunk A/B mirror of document.rs; TF-IDF 2-gram offline retrieval",
        "n_docs": len(docs),
        "n_queries": n_q,
        "results": results,
    }
    with open(args.json, "w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=2)
    print(f"\n结果已写入 {args.json}")

    # 简短结论
    by = {r["strategy"]: r for r in results}
    h = by["heading-aligned"]
    f512 = by["fixed-512"]
    print("\n结论摘要:")
    print(
        f"  heading-aligned vs fixed-512: "
        f"R@1 {h['recall@1']-f512['recall@1']:+.3f}, "
        f"intact {h['intact_rate']-f512['intact_rate']:+.3f}, "
        f"chunks {h['avg_chunks']-f512['avg_chunks']:+.1f}"
    )


if __name__ == "__main__":
    main()
