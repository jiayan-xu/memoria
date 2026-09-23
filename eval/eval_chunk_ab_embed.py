#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
A/B 复验：真实语料 + 嵌入余弦（进程内 sentence_transformers，不依赖 embed_server）
==============================================================================
对照 eval_chunk_ab.py（离线 2-gram）：本脚本用 text2vec-base-chinese 余弦，
语料改为仓库/业务侧真实 Markdown（dashboard / memoria docs）。

指标同前：recall@1/3/5、intact_rate、avg_chunks。
用法：
  python eval/eval_chunk_ab_embed.py
"""
from __future__ import annotations

import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))
# 复用同算法切块
sys.path.insert(0, HERE)
from eval_chunk_ab import (  # noqa: E402
    chunk_heading_aligned,
    chunk_text,
    split_markdown_sections,
)

CORPUS_FILES = [
    # memoria 真实设计文档
    os.path.join(REPO, "docs", "DESIGN_MEMORY_PROFILE_AND_GRAPH.md"),
    os.path.join(REPO, "docs", "MEMORIA_INTEL_HY3_EXEC.md"),
    os.path.join(REPO, "docs", "RECALL_VS_CONSOLIDATE_BOUNDARY.md"),
    os.path.join(REPO, "docs", "INGEST_ADMISSION.md"),
    os.path.join(REPO, "docs", "PR_PROCESS.md"),
    # dashboard 业务文档
    r"C:\Users\user\dashboard\ARCHITECTURE.md",
    r"C:\Users\user\dashboard\business_rules.md",
    r"C:\Users\user\dashboard\schema_design.md",
    r"C:\Users\user\dashboard\manifest_recognizer_design.md",
    r"C:\Users\user\dashboard\EVOLUTION.md",
]


def build_golds_from_sections(text: str, max_golds: int = 6) -> list[dict]:
    """每节取原文中**逐字**连续片段作金标（保证 `gold in chunk` 可判）。"""
    golds = []
    for path, body in split_markdown_sections(text):
        if not path or len(body) < 120:
            continue
        # 在 body 里找一段较长的连续非结构行，按原文切片
        lines = body.splitlines(keepends=True)
        run = []
        best = ""
        for line in lines:
            if line.startswith("#") or line.startswith("```") or line.startswith("|"):
                if len("".join(run)) > len(best):
                    best = "".join(run)
                run = []
                continue
            if line.strip():
                run.append(line)
            else:
                if len("".join(run)) > len(best):
                    best = "".join(run)
                run = []
        if len("".join(run)) > len(best):
            best = "".join(run)
        # 取 80–240 字逐字窗口（含换行原样）
        flat = best.replace("\r\n", "\n")
        if len(flat) < 80:
            continue
        start = 0
        gold = flat[start : start + min(240, len(flat))].strip("\n")
        if len(gold) < 80:
            continue
        title = path[-1]
        golds.append({
            "gold_sentence": gold,
            "section": " › ".join(path),
            "query": f"{title} 的要点和规则",
        })
        if len(golds) >= max_golds:
            break
    return golds


def load_docs() -> list[dict]:
    docs = []
    for fp in CORPUS_FILES:
        if not os.path.isfile(fp):
            continue
        try:
            text = open(fp, encoding="utf-8").read()
        except OSError:
            continue
        if len(text) < 400:
            continue
        golds = build_golds_from_sections(text)
        if len(golds) < 2:
            continue
        docs.append({
            "doc_id": os.path.basename(fp),
            "text": text,
            "golds": golds,
        })
    return docs


def l2(v):
    n = sum(x * x for x in v) ** 0.5
    return [x / n for x in v] if n else v


def cos(a, b):
    return sum(x * y for x, y in zip(a, b))


def evaluate(name, chunker, docs, model) -> dict:
    rec = {1: 0, 3: 0, 5: 0}
    intact = 0
    total = 0
    n_chunks = []
    for doc in docs:
        chunks = chunker(doc["text"])
        n_chunks.append(len(chunks))
        if not chunks:
            continue
        vecs = [l2(v) for v in model.encode(chunks, normalize_embeddings=False)]
        for g in doc["golds"]:
            total += 1
            gold = g["gold_sentence"]
            if any(gold in c for c in chunks):
                intact += 1
            qv = l2(model.encode([g["query"]])[0])
            scores = sorted(
                ((cos(qv, vecs[i]), i) for i in range(len(chunks))),
                key=lambda x: (-x[0], x[1]),
            )
            ranked = [i for _, i in scores[:5]]
            hit_at = None
            for k in (1, 3, 5):
                if hit_at is None and any(gold in chunks[i] for i in ranked[:k]):
                    hit_at = k
            if hit_at:
                for k in (1, 3, 5):
                    if hit_at <= k:
                        rec[k] += 1
    return {
        "strategy": name,
        "n_queries": total,
        "recall@1": round(rec[1] / total, 4) if total else 0,
        "recall@3": round(rec[3] / total, 4) if total else 0,
        "recall@5": round(rec[5] / total, 4) if total else 0,
        "intact_rate": round(intact / total, 4) if total else 0,
        "avg_chunks": round(sum(n_chunks) / len(n_chunks), 2) if n_chunks else 0,
    }


def main():
    print("加载 sentence_transformers (text2vec-base-chinese, offline)...")
    os.environ.setdefault("HF_HUB_OFFLINE", "1")
    os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
    from sentence_transformers import SentenceTransformer

    model = SentenceTransformer(
        os.environ.get("MEMORIA_EMBED_MODEL_LOCAL", "shibing624/text2vec-base-chinese"),
        local_files_only=True,
    )
    docs = load_docs()
    n_q = sum(len(d["golds"]) for d in docs)
    print(f"真实语料: {len(docs)} 文档 / {n_q} 查询")
    for d in docs:
        print(f"  - {d['doc_id']}: {len(d['golds'])} golds, {len(d['text'])} chars")
    if n_q < 4:
        print("语料不足，中止")
        sys.exit(1)

    strategies = [
        ("fixed-512", lambda t: chunk_text(t, 512)),
        ("fixed-3500", lambda t: chunk_text(t, 3500)),
        ("heading-aligned", lambda t: chunk_heading_aligned(t, 3500)),
    ]
    results = []
    print(f"\n{'strategy':<18} {'R@1':>6} {'R@3':>6} {'R@5':>6} {'intact':>8} {'chunks':>8}")
    for name, fn in strategies:
        r = evaluate(name, fn, docs, model)
        results.append(r)
        print(
            f"{r['strategy']:<18} {r['recall@1']:>6} {r['recall@3']:>6} {r['recall@5']:>6} "
            f"{r['intact_rate']:>8} {r['avg_chunks']:>8}"
        )

    out = os.path.join(HERE, "chunk_ab_embed_result.json")
    with open(out, "w", encoding="utf-8") as f:
        json.dump({"backend": "text2vec-base-chinese-cosine", "n_docs": len(docs), "n_queries": n_q, "results": results}, f, ensure_ascii=False, indent=2)
    print(f"\n结果: {out}")

    by = {r["strategy"]: r for r in results}
    h, f5, f35 = by["heading-aligned"], by["fixed-512"], by["fixed-3500"]
    print("\n结论摘要:")
    print(f"  heading vs fixed-512 : R@1 {h['recall@1']-f5['recall@1']:+.3f}  R@5 {h['recall@5']-f5['recall@5']:+.3f}  chunks {h['avg_chunks']-f5['avg_chunks']:+.1f}")
    print(f"  heading vs fixed-3500: R@1 {h['recall@1']-f35['recall@1']:+.3f}  R@5 {h['recall@5']-f35['recall@5']:+.3f}  chunks {h['avg_chunks']-f35['avg_chunks']:+.1f}")


if __name__ == "__main__":
    main()
