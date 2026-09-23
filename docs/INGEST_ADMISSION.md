# 文档入库格式准入（P0 纪律）

> 2026-09-23 · 配合「纯 MD 文档层 + 原子事实记忆层 + 原件旁挂」契约  
> 研究依据：`research/pure-md-memory-claim/`

## 契约（三层）

| 层 | 载体 | 进 content？ | 路径 |
|----|------|--------------|------|
| **文档层** | 干净 MD / 保结构文本 | ✅ | `ingest_document` / `ingest_plain_text` → `memory_type=document` 清单+分块 |
| **事实层** | 原子事实 / 决策 / 实体 | ✅ | `memory_remember` + `parent_id`/`raw_ref` |
| **保真层** | PDF / DOCX / XLSX 原件 | ❌ | `raw_ref` / `data/documents/...` |

## 格式准入表

| 来源形态 | 处理 | 入库形态 |
|----------|------|----------|
| 已抽 MD / TXT | 直接 `ingest_plain_text` | 标题对齐分块 + 面包屑 |
| PDF/DOCX 可选中文字 | 一次解析 → 落 MD/文本 + 原件旁挂 | 文档层 |
| Excel / 表格 | 转 TSV（DateTime 必须可读日期） | `# Sheet` 标题段 + **行边界切块**（表头重复） |
| 数字考核 / 合并单元格表 | 优先 CSV/JSON 或保布局文本；MD 管道表仅作索引 | 文档层 + 结构化事实 |
| 法定 / 盖章 / 签字页 | **原件为准**；抽取文只作检索索引 | 保真层 + 可选文档层 |
| 扫描件 / 纯图 PDF | 当前拒绝（无 OCR）；勿硬塞垃圾文本 | — |
| 表单 / 嵌套表 / 公式 | 不硬转 MD；原生格式工具 | 保真层 |

## 硬纪律

1. **解析一次落盘**——禁止用 LLM 反复「洗」抽取文本（会把对的改坏）。
2. **原件不进 `content`**——只进 `raw_ref`。
3. **HyPE 只喂干净句**——`is_hyde_worthy` 门控：表格行 / OCR 噪声 / 代码 / 过短不生成假设问句。
4. **禁止**把扫描件 OCR 噪声直接当记忆正文。
5. 表格失真（列错位、小数丢失、合并单元格）场景，**数字以原件/结构表为准**，MD 块只服务检索。

## 实现挂钩

- 标题对齐切块 + 面包屑：`src/document.rs` `chunk_text_structured`
- 表格行保真切块：`chunk_preserving_rows`（表头每块重复）
- Excel DateTime：`format_excel_serial`
- HyPE 干净度门：`tools/offline/build_hype_vectors.py` `is_hyde_worthy`
