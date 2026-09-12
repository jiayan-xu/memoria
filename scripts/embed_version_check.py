#!/usr/bin/env python
"""Embedding 版本记账 — 记录当前向量模型/维度，检测不匹配。

用法：python scripts/embed_version_check.py
状态存 data/embed_version.json（不改 migration_flags schema）。
"""
import json
import sqlite3
import sys
from datetime import datetime
from pathlib import Path

DB = Path(r"C:\services\memoria\data\memoria.db")
STATE_FILE = Path(r"C:\services\memoria\data\embed_version.json")
EXPECTED_MODEL = "bge-m3"
EXPECTED_DIMS = 1024


def vector_dims(blob):
    if not blob or len(blob) < 4:
        return 0
    return len(blob) // 4


def main() -> int:
    if not DB.is_file():
        print(f"✗ DB 不存在: {DB}")
        return 1

    # 读实际向量维度
    conn = sqlite3.connect(str(DB))
    row = conn.execute("SELECT vector FROM memory_vectors LIMIT 1").fetchone()
    actual_dims = (len(row[0]) // 4) if row and row[0] else 0
    vec_count = conn.execute("SELECT COUNT(*) FROM memory_vectors").fetchone()[0]
    conn.close()

    # 读/写版本记录
    state = {}
    if STATE_FILE.is_file():
        state = json.loads(STATE_FILE.read_text(encoding="utf-8"))

    stored_model = state.get("model")
    stored_dims = state.get("dims")

    print("=== Embedding 版本状态 ===")
    print(f"  期望: {EXPECTED_MODEL} / {EXPECTED_DIMS} dims")
    print(f"  记录: {stored_model or '(未记录)'} / {stored_dims or '(未记录)'} dims")
    print(f"  实际向量维度: {actual_dims} | 向量数: {vec_count}")

    mismatch = (actual_dims > 0 and actual_dims != EXPECTED_DIMS)
    if stored_model and stored_model != EXPECTED_MODEL:
        mismatch = True

    # 更新记录
    state.update({
        "model": EXPECTED_MODEL, "dims": EXPECTED_DIMS,
        "checked_at": datetime.now().isoformat(timespec="seconds"),
        "mismatch": mismatch, "vector_count": vec_count,
    })
    STATE_FILE.write_text(json.dumps(state, ensure_ascii=False, indent=1), encoding="utf-8")

    if mismatch:
        print(f"  ⚠ 维度不匹配: 实际 {actual_dims} ≠ 期望 {EXPECTED_DIMS} → 需跑 rebuild_vectors.py")
        return 1
    print(f"  ✓ 无不匹配（{vec_count} 个向量）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
