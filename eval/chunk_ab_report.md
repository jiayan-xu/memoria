# 切块策略 A/B：heading-aligned vs fixed

> 2026-09-23 · harness: `eval/eval_chunk_ab.py` · 结果: `eval/chunk_ab_result.json`  
> 镜像 `src/document.rs` 切块算法；离线字符 2-gram TF-IDF，无嵌入服务依赖

## 问题

文档层切块：按 Markdown 标题边界（heading-aligned）是否优于固定字符切（fixed-512 / fixed-3500）？

## 设置

| 项 | 值 |
|----|-----|
| 语料 | 3 份合成多章节 MD（含超长附录表）+ 3 份 `docs/*.md` |
| 查询 | 16 条（标题导向「主题+节名+完整规则」） |
| 金标 | 整段规则文字（含长段，测能否被撕开） |
| 策略 | fixed-512（overlap 200）/ fixed-3500 / heading-aligned（3500 上限） |
| 指标 | recall@1/3/5、intact_rate、avg_chunks |

## 结果

| strategy | R@1 | R@3 | R@5 | intact | avg_chunks |
|----------|-----|-----|-----|--------|------------|
| fixed-512 | 0.500 | 0.875 | 0.938 | 1.00 | 25.3 |
| fixed-3500 | 0.750 | 1.000 | 1.000 | 1.00 | 2.8 |
| **heading-aligned** | **0.875** | 0.938 | **1.000** | 1.00 | **13.3** |

**heading-aligned vs fixed-512：R@1 +0.375，块数 −12**

## 解读（对照研究结论）

1. **结构对齐对 R@1 有实质增益**，尤其相对小固定块（512）：+37.5pp。
2. 相对 **fixed-3500**：R@1 +12.5pp，R@3 略低（0.938 vs 1.00），但块数 13.3 vs 2.8——**粒度更利于证据定位**，大块靠「块内塞下更多噪声」抬 R@3。
3. 与 F3 文献一致：**收益排序 ≈ 是否切开/保留结构 > 具体边界算法**；fixed-512 最差，heading 与大 fixed 接近且更优。
4. **intact_rate 全 1.0**：overlap=200 使中短金段仍能完整落入某窗；长段撕开需金文 >312 步长才稳定复现，本集未稳定拉开。不代表 heading 无防撕开价值——表格行保真切块的单测已覆盖该路径。

## 建议

- **文档层默认 heading-aligned**（已实现），`chunk_chars=3500`。
- 不要为省事退回 fixed-512（R@1 显著差）。
- 若追求最小块数可接受 fixed-3500，但证据定位与 HyPE 喂入粒度不如标题块干净。
- 后续可加：真实部门文档语料 + embed_server 余弦，复验同一结论。

## 复现

```powershell
python eval\eval_chunk_ab.py
```
