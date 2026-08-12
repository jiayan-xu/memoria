#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
V1（2026-08-12）：HyPE 假设问句补嵌脚本（A/B 验证用）。

对 golden set（eval/hyde_queries.json）的记忆生成「假设问句」并嵌入，写入
memory_hype_vectors 表（与 memory_vectors 平行）。semantic_search 双路合并后
可验证 HyPE 是否提升 recall（论文：IEEE Access 2025, +42pp precision）。

关键纪律：
- **绝不用 golden 的 query 作为假设问句**（那是评测集，直接用了 = 数据泄漏/作弊）。
  假设问句由 LLM 独立生成（模拟「用户会怎么问」），与 golden query 措辞不同。
- 生成失败/嵌入失败 → 跳过该条（记日志），不中断、不伪造。
- 幂等：已存在 id 的 hype 向量会 UPDATE（重跑安全）。
- 只写新表，不动 memory_vectors / memories / HNSW 内容索引。

用法：
  python tools/offline/build_hype_vectors.py            # 默认 golden set 58 条
  python tools/offline/build_hype_vectors.py --limit 10 # 只处理前 10 条（调试）
  python tools/offline/build_hype_vectors.py --all      # 全库 5339 条（验证通过后）
环境：SILICONFLOW_API_KEY（.env 或环境变量）；复用 embed_server 的 chat/embed 通道。
"""
import os
import sys
import json
import time
import struct
import sqlite3
import urllib.request
import argparse

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, "..", ".."))
# 默认 DB 相对仓库内 data/（与代码库其余脚本一致），而非依赖外层目录恰好叫
# "memoria"——否则换目录名会静默写到错误的兄弟路径（#R35 maintainability/low）。
DB = os.environ.get(
    "MEMORIA_DB_PATH",
    os.path.join(REPO, "data", "memoria.db"),
)
GOLDEN = os.path.join(REPO, "eval", "hyde_queries.json")
# .env 优先环境变量，其次仓库同级 memoria/ 的运行时镜像（与 recall_guard 的 resolve 一致）
ENV = os.environ.get(
    "MEMORIA_ENV_PATH",
    os.path.join(os.path.dirname(REPO), "memoria", ".env"),
)

EMBED_URL = os.environ.get("MEMORIA_EMBEDDING_URL", "http://127.0.0.1:8777/embed")
# Rust 侧 HNSW 维度硬约束（src/vector/hnsw.rs DIM=1024）。仅校验 embed_server 返回的 dim
# 不够——服务器可能跑 local 模型（text2vec 768d），len(v)==dim 通过但 rebuild 侧
# v.len()!=DIM 会静默跳过每一行，整批"看起来成功"而索引恒空（#R34 bug/medium）。
RUST_DIM = int(os.environ.get("MEMORIA_EMBED_DIM", "1024"))
CHAT_URL = os.environ.get(
    "SILICONFLOW_CHAT_URL", "https://api.siliconflow.cn/v1/chat/completions"
)
CHAT_MODEL = os.environ.get("MEMORIA_HYDE_MODEL", "Qwen/Qwen2.5-7B-Instruct")


def get_secret(name, path):
    try:
        for line in open(path, encoding="utf-8-sig"):
            line = line.strip()
            if line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            if k.strip() == name:
                return v.strip().strip('"').strip("'")
    except FileNotFoundError:
        pass
    return None


SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "") or get_secret("SILICONFLOW_API_KEY", ENV) or ""


def generate_question(content: str) -> str:
    """LLM 生成「用户会怎么问才能找到这条记忆」的假设问句（与 golden query 独立）。"""
    sys_prompt = (
        "你是一个记忆检索优化器。给定一条知识库中的事实/记忆文本，请写一句"
        "「用户将来会怎么提问来检索这条信息」的自然问句。要求：\n"
        "1) 用口语化、自然的方式提问，不要照搬原文措辞；\n"
        "2) 问句要具体，能通过这条记忆回答；\n"
        "3) 只输出这一句问句，不要解释、不要引号、不要编号。\n"
        "若原文明显不可检索（纯代码/乱码），输出空字符串。"
    )
    body = {
        "model": CHAT_MODEL,
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
            req = urllib.request.Request(
                CHAT_URL, data=json.dumps(body).encode(), headers=headers, method="POST"
            )
            with urllib.request.urlopen(req, timeout=30) as r:
                resp = json.loads(r.read().decode())
            return resp["choices"][0]["message"]["content"].strip().strip('"').strip("'")
        except Exception as e:
            if attempt == 2:
                print(f"  [warn] chat 失败: {e}")
                return ""
            time.sleep(min(2 ** attempt, 10))


def embed(texts):
    """调本地 embed_server（siliconflow Qwen3-VL-8B），返回 (vectors, dim)。

    与 generate_question 同款重试纪律：全库 --all 一次跑 5339 行，单 POST 无重试时
    任何瞬时网络/服务抖动都会永久漏掉那些记忆（--all 缺口无法定点修复）。
    3 次尝试 + 指数退避；最终失败抛异常由调用方计数（并记录失败 id，见 fail_ids）。
    """
    body = {"texts": texts, "normalize": False}
    headers = {"Content-Type": "application/json"}
    last_err = None
    for attempt in range(3):
        try:
            req = urllib.request.Request(
                EMBED_URL, data=json.dumps(body).encode(), headers=headers, method="POST"
            )
            with urllib.request.urlopen(req, timeout=60) as r:
                resp = json.loads(r.read().decode())
            return resp["embeddings"], resp.get("dim", 1024)
        except Exception as e:
            last_err = e
            if attempt < 2:
                time.sleep(min(2 ** attempt, 10))
    raise last_err


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=None, help="只处理前 N 条")
    ap.add_argument("--all", action="store_true", help="全库补嵌（验证通过后）")
    ap.add_argument("--db", default=DB)
    ap.add_argument("--dry-run", action="store_true", help="只生成问句不写库")
    args = ap.parse_args()

    if not SF_KEY:
        print("错误: SILICONFLOW_API_KEY 未配置（.env 或环境变量）")
        sys.exit(1)

    # ── 目标记忆列表 ──
    if args.all:
        con = sqlite3.connect(args.db)
        rows = con.execute(
            "SELECT id, content FROM memories WHERE namespace='agent/xujiayan' "
            "AND superseded_by IS NULL AND length(content) BETWEEN 10 AND 500"
        ).fetchall()
        con.close()
        targets = [(mid, c) for mid, c in rows]
        print(f"全库目标: {len(targets)} 条")
    else:
        d = json.load(open(GOLDEN, encoding="utf-8"))
        targets = [(it["id"], it["content"]) for it in d["items"]]
        print(f"golden 目标: {len(targets)} 条")

    if args.limit is not None:
        targets = targets[: args.limit]

    # connect + DDL 移到 dry-run 之后（#R36 bug/medium）：dry-run 承诺"只生成问句不写库"，
    # 但 sqlite3.connect 会创建不存在的 DB 文件、CREATE TABLE/INDEX 会真实执行——
    # 对生产库跑验证性 dry-run 也会产生副作用，与"完成(dry-run，未写库)"文案矛盾。
    con = None
    if not args.dry_run:
        con = sqlite3.connect(args.db)
        # 自包含：表可能尚未经 Rust 迁移创建（离线补嵌先于服务启动时）——镜像
        # src/storage/sqlite.rs 的 schema，确保首次运行不因缺表崩溃、幂等成立。
        con.execute(
            "CREATE TABLE IF NOT EXISTS memory_hype_vectors ("
            "id TEXT PRIMARY KEY, namespace TEXT NOT NULL DEFAULT 'default', "
            "question TEXT, vector BLOB NOT NULL, updated_at TEXT DEFAULT (datetime('now')))"
        )
        con.execute(
            "CREATE INDEX IF NOT EXISTS idx_hype_ns ON memory_hype_vectors(namespace)"
        )
    ok = skip = fail = 0
    fail_ids = []
    for i, (mid, content) in enumerate(targets, 1):
        q = generate_question(content)
        if not q or len(q) < 6:
            print(f"[{i}/{len(targets)}] {mid[:8]} 问句生成失败/过短，跳过")
            skip += 1
            # 跳过也记入 fail_ids：hype_failed_ids.txt 承诺"可定点重跑"——LLM 瞬时失败
            # 的记忆若不记录，基于该文件的定点重跑永远无法补上这些缺口（#R34
            # maintainability/low）。
            fail_ids.append(mid)
            continue
        # dry-run 前置：只生成问句、不嵌入不写库——文档承诺"只生成问句"就必须
        # 不依赖 embed_server 在线（否则服务抖动时 dry-run 报"嵌入失败"而非显示问句）。
        if args.dry_run:
            print(f"[{i}/{len(targets)}] {mid[:8]} [dry] 问句: {q[:60]}")
            ok += 1
            continue
        try:
            vecs, dim = embed([q])
            v = vecs[0]
            # 维度校验与 pack 也在 try 内：畸形响应（非序列/非数值）抛异常 →
            # 按失败计数跳过而非中断整个 --all 批（否则 5339 条白跑且无 fail_ids）。
            # 同时校验 embed_server 的 dim 与 Rust 侧 DIM 一致：服务器若跑 local 模型
            # （768d），len(v)==dim 会通过但 rebuild 侧全部静默跳过——必须显式拒绝。
            if dim != RUST_DIM:
                raise ValueError(f"服务维度 {dim}≠Rust DIM {RUST_DIM}（模型不匹配）")
            if len(v) != dim:
                raise ValueError(f"维度异常 {len(v)}≠{dim}")
            blob = struct.pack(f"<{len(v)}f", *v)
            # 写入也在此 try 内（#R35 bug/medium）：服务运行中 DB 可能被锁/磁盘满，
            # 单行写失败若抛到外层会中断整批且 fail_ids 永不落盘——违背
            # "不中断、可定点重跑"承诺。写失败按失败计数跳过并记录 id。
            con.execute(
                "INSERT OR REPLACE INTO memory_hype_vectors (id, namespace, question, vector, updated_at) "
                "VALUES (?, 'agent/xujiayan', ?, ?, datetime('now'))",
                (mid, q, blob),
            )
            con.commit()
        except Exception as e:
            print(f"[{i}/{len(targets)}] {mid[:8]} 嵌入/写入异常: {e}")
            fail += 1
            fail_ids.append(mid)
            continue
        ok += 1
        if i % 10 == 0:
            print(f"  ...{i}/{len(targets)}")

    if con is not None:
        con.close()
    if args.dry_run:
        # dry-run 不落库——文案须如实反映，否则运维误以为已写入（#R34 other/low）
        print(f"\n完成(dry-run，未写库): 生成 {ok} / 跳过 {skip} / 失败 {fail}")
        return
    print(f"\n完成: 写入 {ok} / 跳过 {skip} / 失败 {fail}")
    if fail_ids:
        # 失败 id 落盘（ops 可据此定点重跑，不必全量 --all 再来一遍）
        with open(os.path.join(HERE, "hype_failed_ids.txt"), "w", encoding="utf-8") as f:
            f.write("\n".join(fail_ids))
        print(f"失败 id 已写入 hype_failed_ids.txt（{len(fail_ids)} 条）")


if __name__ == "__main__":
    main()
