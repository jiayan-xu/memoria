#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
云端全量重嵌脚本（SiliconFlow Qwen3-VL-Embedding-8B）
=====================================================

将 memoria 的 memories 全量用云端 embedding 重新向量化，写入 memory_vectors，
使 HNSW 语义检索切换到新模型。设计目标：
  - 零 Rust 改动（默认 dim=768，对接现有 hnsw.rs:DIM=768）
  - 重嵌前自动备份 memory_vectors（可回退）
  - 分批 + 限流重试（429 指数退避）
  - 断点续传（--only-missing 只填缺失；--limit 试跑小样本）
  - 进度 + 剩余时间估计

用法：
  # 试跑 200 条（不覆盖全量，验证链路）
  python cloud_backfill.py --limit 200

  # 全量重嵌（先备份，再覆盖）
  python cloud_backfill.py --backup

  # 只补缺失（断点续传）
  python cloud_backfill.py --only-missing

注意：
  - 运行前务必停掉 memoria-server + 旧 embed 服务（避免写库冲突 / 向量空间混合）。
  - 全量约 42336 条，按 SiliconFlow 限流可能需数十分钟，建议后台运行。
  - 维度由 --dim 控制（默认 768）；若用 1024/4096 须同步改 hnsw.rs:DIM 并重编 memoria。
"""

import os
import sys
import time
import struct
import argparse
import sqlite3
import requests

# ── 配置 ──
SF_API = os.environ.get("SILICONFLOW_API_URL", "https://api.siliconflow.cn/v1/embeddings")
SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "")
DEFAULT_MODEL = os.environ.get("MEMORIA_EMBED_MODEL", "Qwen/Qwen3-VL-Embedding-8B")
DEFAULT_DB = os.path.expanduser(r"~/.qclaw/workspace/memoria/data/memoria.db")
# ⚠️ SiliconFlow Qwen3-VL-Embedding-8B 的 data[].index 字段在 batch>8 时只返回 0..7
# （batch=32 时每个 index 重复 4 次、batch=16 重复 2 次）。若按 index 映射，只有前 8 槽
# 被写入（且被覆盖）、8..31 槽保持 None → 之前被静默零填充，造成 75% 零向量事故。
# 故批大小必须 ≤8（batch=8 时 index 恰为 0..7 唯一且正确）。已实测验证（2026-07-24）。
BATCH = 8


def _embed_single(text, model, dim, retries=3):
    """单条兜底重试；返回 embedding 或 None（不抛异常，由上层决定跳过/上报）。"""
    if not SF_KEY or not (text or "").strip():
        return None
    headers = {"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"}
    body = {"model": model, "input": [text], "encoding_format": "float", "dimensions": dim}
    for attempt in range(retries):
        try:
            r = requests.post(SF_API, headers=headers, json=body, timeout=60)
            if r.status_code == 200:
                data = r.json().get("data", [])
                if data and data[0].get("embedding"):
                    return data[0]["embedding"]
                return None
            elif r.status_code in (429, 503, 504):
                time.sleep(min(2 ** attempt, 15))
                continue
            else:
                return None
        except requests.RequestException:
            time.sleep(min(2 ** attempt, 15))
    return None


def embed_sf(texts, model, dim):
    """调 SiliconFlow，分批(≤8) + 限流重试。返回 list，长度=len(texts)；失败槽保留 None（绝不零填充）。"""
    if not SF_KEY:
        raise RuntimeError("SILICONFLOW_API_KEY 未设置")
    headers = {"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"}
    out = [None] * len(texts)
    for i in range(0, len(texts), BATCH):
        batch = texts[i : i + BATCH]
        body = {"model": model, "input": batch, "encoding_format": "float", "dimensions": dim}
        for attempt in range(7):
            try:
                r = requests.post(SF_API, headers=headers, json=body, timeout=60)
                if r.status_code == 200:
                    for item in r.json().get("data", []):
                        idx = item.get("index", 0)
                        if i + idx < len(out):
                            out[i + idx] = item["embedding"]
                    break
                elif r.status_code in (429, 503, 504):
                    time.sleep(min(2 ** attempt, 30))
                    continue
                else:
                    raise RuntimeError(f"SF {r.status_code}: {r.text[:200]}")
            except requests.RequestException:
                time.sleep(min(2 ** attempt, 30))
                if attempt == 6:
                    raise RuntimeError("SF 请求失败（重试耗尽）")
    # 兜底：任何 None 槽逐条单独重试；仍失败则保留 None，绝不写零向量（调用方跳过+上报）。
    for j, v in enumerate(out):
        if v is None:
            out[j] = _embed_single(texts[j], model, dim)
    return out


def backup_table(conn, table, ts):
    bak = f"{table}_bak_{ts}"
    conn.execute(f"DROP TABLE IF EXISTS {bak}")
    conn.execute(f"CREATE TABLE {bak} AS SELECT * FROM {table}")
    conn.commit()
    print(f"[backup] {table} -> {bak}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db-path", default=DEFAULT_DB)
    ap.add_argument("--dim", type=int, default=768)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--limit", type=int, default=0, help="试跑条数（0=全部）")
    ap.add_argument("--only-missing", action="store_true", help="只填 memory_vectors 缺失的")
    ap.add_argument("--only-zero", action="store_true", help="只重嵌 memory_vectors 中零向量的（修复 batch>8 index bug 产生的零向量）")
    ap.add_argument("--backup", action="store_true", help="重嵌前备份 memory_vectors")
    args = ap.parse_args()

    if not SF_KEY:
        print("[FATAL] SILICONFLOW_API_KEY 未设置", file=sys.stderr)
        sys.exit(1)

    ts = time.strftime("%Y%m%d_%H%M%S")
    conn = sqlite3.connect(args.db_path)
    cur = conn.cursor()

    # 自适应列名
    cur.execute("PRAGMA table_info(memories)")
    cols = [r[1] for r in cur.fetchall()]
    content_col = "content" if "content" in cols else ("text" if "text" in cols else "body")
    ns_col = "namespace" if "namespace" in cols else None

    # 备份
    if args.backup:
        backup_table(conn, "memory_vectors", ts)

    # 取待重嵌的记忆
    if args.only_zero:
        # 选出 memory_vectors 中已存在但向量为零（范数≈0）的记忆 → 重嵌修复
        if ns_col:
            cur.execute(f"SELECT m.id, m.{ns_col}, m.{content_col}, v.vector FROM memories m "
                        f"JOIN memory_vectors v ON m.id=v.id")
        else:
            cur.execute(f"SELECT m.id, 'agent/xujiayan', m.{content_col}, v.vector FROM memories m "
                        f"JOIN memory_vectors v ON m.id=v.id")
        rows = []
        for mid, ns, content, vblob in cur.fetchall():
            try:
                n = len(vblob) // 4
                vec = struct.unpack("<%df" % n, vblob)
                if sum(x * x for x in vec) < 1e-12:
                    rows.append((mid, ns, content))
            except Exception:
                rows.append((mid, ns, content))  # 解码异常也重嵌
    elif args.only_missing:
        if ns_col:
            cur.execute(f"SELECT m.id, m.{ns_col}, m.{content_col} FROM memories m "
                        f"LEFT JOIN memory_vectors v ON m.id=v.id WHERE v.id IS NULL")
        else:
            cur.execute(f"SELECT m.id, 'agent/xujiayan', m.{content_col} FROM memories m "
                        f"LEFT JOIN memory_vectors v ON m.id=v.id WHERE v.id IS NULL")
        rows = cur.fetchall()
    else:
        if ns_col:
            cur.execute(f"SELECT id, {ns_col}, {content_col} FROM memories")
        else:
            cur.execute(f"SELECT id, 'agent/xujiayan', {content_col} FROM memories")
        rows = cur.fetchall()
    if args.limit:
        rows = rows[: args.limit]
    print(f"[info] 待重嵌 {len(rows)} 条 (dim={args.dim}, model={args.model})", flush=True)

    done = 0
    failed = []
    t0 = time.time()
    for i in range(0, len(rows), BATCH):
        chunk = rows[i : i + BATCH]
        ids = [r[0] for r in chunk]
        nss = [r[1] for r in chunk]
        texts = [r[2] for r in chunk]
        vecs = embed_sf(texts, args.model, args.dim)
        for mid, ns, vec in zip(ids, nss, vecs):
            if vec is None:
                failed.append(mid)  # 跳过，绝不写零向量；可 --only-missing/--only-zero 续补
                continue
            blob = struct.pack("<" + "f" * args.dim, *vec)
            cur.execute(
                "INSERT OR REPLACE INTO memory_vectors(id, namespace, vector, updated_at) VALUES (?,?,?,?)",
                (mid, ns, blob, time.strftime("%Y-%m-%dT%H:%M:%S")),
            )
        conn.commit()
        done += len(chunk)
        el = time.time() - t0
        rate = done / el if el > 0 else 0
        eta = (len(rows) - done) / rate if rate > 0 else 0
        if done % 500 < BATCH or done == len(rows):
            print(f"[progress] {done}/{len(rows)} ({done*100//len(rows)}%) "
                  f"rate={rate:.1f}/s eta={eta:.0f}s", flush=True)

    conn.close()
    print(f"[done] 重嵌完成 {done} 条，耗时 {time.time()-t0:.0f}s，失败/跳过 {len(failed)} 条", flush=True)
    if failed:
        print(f"[warn] {len(failed)} 条未成功嵌入（已跳过，未写零向量），可用 --only-missing 续补", flush=True)


if __name__ == "__main__":
    main()
