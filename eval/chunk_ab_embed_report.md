# 切块 A/B 复验：真实语料 + 嵌入余弦

> 2026-09-23 · harness: `eval/eval_chunk_ab_embed.py`  
> 对照：`eval/chunk_ab_report.md`（离线 2-gram + 合成语料）  
> 后端：`shibing624/text2vec-base-chinese` 768d 进程内余弦（`local_files_only`）

## 语料（真实 Markdown）

| 文档 | golds | chars |
|------|-------|-------|
| DESIGN_MEMORY_PROFILE_AND_GRAPH.md | 6 | 26699 |
| MEMORIA_INTEL_HY3_EXEC.md | 6 | 8261 |
| RECALL_VS_CONSOLIDATE_BOUNDARY.md | 6 | 3505 |
| INGEST_ADMISSION.md | 2 | 1338 |
| PR_PROCESS.md | 6 | 2751 |
| dashboard/ARCHITECTURE.md | 6 | 5978 |
| dashboard/business_rules.md | 6 | 3108 |
| dashboard/schema_design.md | 3 | 2403 |
| dashboard/manifest_recognizer_design.md | 6 | 7765 |
| dashboard/EVOLUTION.md | 6 | 8672 |
| **合计** | **53 查询** | **10 文档** |

金标 = 节内**逐字** 80–240 字片段（`gold in chunk` 可判）。

## 结果

| strategy | R@1 | R@3 | R@5 | intact | avg_chunks |
|----------|-----|-----|-----|--------|------------|
| fixed-512 | 0.302 | 0.604 | 0.698 | 0.98 | 22.5 |
| fixed-3500 | 0.491 | 0.962 | 0.981 | 1.00 | 2.6 |
| **heading-aligned** | **0.566** | 0.792 | 0.830 | **1.00** | 21.4 |

## 与离线 A/B 对照

| 对比 | 离线 2-gram（合成） | 嵌入余弦（真实） | 是否一致 |
|------|---------------------|------------------|----------|
| heading vs fixed-512 R@1 | +0.375 | **+0.264** | ✅ 同向，真实集幅度略小 |
| heading vs fixed-3500 R@1 | +0.125 | **+0.075** | ✅ 同向 |
| heading vs fixed-3500 R@5 | 0 | **−0.151** | ⚠️ 新发现：大块「碰巧含金标」更高 |
| intact | 全 1.0（overlap 护住） | heading 1.0；fixed-512 0.98 | ✅ heading 防撕开 |

## 结论（复验后修正）

1. **heading-aligned 仍是文档层默认**：R@1 在真实语料上仍最高（0.566），相对 fixed-512 **+26.4pp**，与离线结论同向。
2. **fixed-3500 的 R@5 高是「大块装得多」**，不是定位更准——R@1 仍低于 heading；对 HyPE/引用/证据定位不友好。
3. **fixed-512 再次证伪**：R@1/R@5 双低，块数还最多。
4. **intact**：真实语料上 heading 100%，fixed-512 有撕开（0.98）——防撕开优势在真实文本上出现了。

## 局限

- 查询为「节标题 + 的要点和规则」模板，非真实用户问句；可能高估标题对齐优势。
- 金标按节切片，偏「章节定位」任务。
- 单嵌入模型（text2vec 768d），未用 bge-m3/硅基流动 1024d。

## 建议（维持并微调）

- **文档层默认 heading-aligned**（A/B 两轮均支持）。
- 不回退 fixed-512。
- 若只求「块内碰巧装下答案」可用大 fixed，但产品要的是**定位**，仍选 heading。
- 后续可选：真实用户 query 日志 + bge-m3 复验。

## 复现

```powershell
$env:HF_HUB_OFFLINE='1'
python eval\eval_chunk_ab_embed.py
```
