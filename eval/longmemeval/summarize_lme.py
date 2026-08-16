#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""LongMemEval 结果汇总：从 longmemeval_results.json 生成报告 §3 的统计与 markdown 表格。

用法: python summarize_lme.py [results.json]
输出: 终端统计 + 可直接粘贴进 longmemeval_report.md §3 的 markdown。
"""
import json
import os
import sys

sys.stdout.reconfigure(encoding="utf-8")

HERE = os.path.dirname(os.path.abspath(__file__))
PATH = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "longmemeval_results.json")

with open(PATH, encoding="utf-8") as f:
    results = json.load(f)

scored = [r for r in results if r.get("score") is not None]
total = len(results)
n_scored = len(scored)
avg = sum(r["score"] for r in scored) / n_scored if n_scored else 0.0
pass8 = sum(1 for r in scored if r["score"] >= 8)
hit = sum(1 for r in results if r.get("answer_hit"))
unknown = [r for r in results if str(r.get("pred", "")).strip().upper() == "UNKNOWN"]

print("== 总体 ==")
print(f"总 QA: {total}   评分: {n_scored}   跳过: {total - n_scored}")
print(f"平均分: {avg:.2f}")
print(f"pass@8 占比: {100 * pass8 / n_scored:.1f}%  ({pass8}/{n_scored})")
print(f"证据命中率(answer session 进 top-8): {100 * hit / total:.1f}%  ({hit}/{total})")
print(f"UNKNOWN 回答: {len(unknown)}")
print(f"平均 retrieved: {sum(r.get('retrieved', 0) for r in results) / total:.1f}")

print("\n== 按 question_type ==")
by_type = {}
for r in results:
    by_type.setdefault(r["question_type"], []).append(r)
print("| question_type | n | 平均分 | pass@8 | 命中率 | UNKNOWN |")
print("|---|---|---|---|---|---|")
for qt, rs in sorted(by_type.items()):
    ss = [r["score"] for r in rs if r.get("score") is not None]
    hs = sum(1 for r in rs if r.get("answer_hit"))
    un = sum(1 for r in rs if str(r.get("pred", "")).strip().upper() == "UNKNOWN")
    if ss:
        print(f"| {qt} | {len(rs)} | {sum(ss)/len(ss):.2f} | "
              f"{100*sum(1 for s in ss if s >= 8)/len(ss):.0f}% | {100*hs/max(len(rs),1):.0f}% | {un} |")

print("\n== 按能力域 ==")
by_cap = {}
for r in results:
    by_cap.setdefault(r["capability"], []).append(r)
print("| 能力域 | n | 平均分 | pass@8 | 命中率 |")
print("|---|---|---|---|---|")
for cap, rs in sorted(by_cap.items()):
    ss = [r["score"] for r in rs if r.get("score") is not None]
    hs = sum(1 for r in rs if r.get("answer_hit"))
    if ss:
        print(f"| {cap} | {len(rs)} | {sum(ss)/len(ss):.2f} | "
              f"{100*sum(1 for s in ss if s >= 8)/len(ss):.0f}% | {100*hs/max(len(rs),1):.0f}% |")

print("\n== 分数分布 ==")
buckets = {}
for r in scored:
    b = int(r["score"])
    buckets[b] = buckets.get(b, 0) + 1
for b in sorted(buckets):
    print(f"  score={b}: {buckets[b]}")
