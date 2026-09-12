#!/usr/bin/env python
"""Memoria 每日备份 — 导出 JSONL + DB 快照 + 恢复演练。

用法：
  python scripts/memoria_backup.py             # 执行备份
  python scripts/memoria_backup.py --verify    # 恢复演练（临时目录中验证可 recall）

备份策略：
  1. memory_export JSONL → C:/services/backups/memoria/memoria_export_YYYYMMDD.jsonl
  2. DB 文件拷贝（WAL checkpoint 后）    → C:/services/backups/memoria/memoria_db_YYYYMMDD.db
  3. 保留最近 14 份，旧备份自动清理
  4. --verify 在临时目录中用 JSONL 重建并抽检

注意：本阶段备份与本机同盘；后续迁移到网络共享或外置存储时改 BACKUP_DIR 即可。
"""
import argparse
import csv
import json
import os
import shutil
import sqlite3
import sys
from datetime import datetime, timedelta
from pathlib import Path

DB = Path(r"C:\services\memoria\data\memoria.db")
BACKUP_DIR = Path(r"C:\services\backups\memoria")
KEEP_DAYS = 14
EXPORT_TOOL_NS = "agent/xujiayan"  # 主要业务 ns


def checkpoint_wal(db_path: Path) -> None:
    """WAL 模式下先 checkpoint 再拷贝，确保 DB 文件一致性。"""
    conn = sqlite3.connect(str(db_path))
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.close()


def export_jsonl(db_path: Path, out_path: Path) -> int:
    """直接从 DB 导出 JSONL（比走 MCP 更快且不依赖网络）。"""
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    count = 0
    with open(out_path, "w", encoding="utf-8") as f:
        # 逐表导出核心数据
        for table in ("memories", "vectors"):
            try:
                cur = conn.execute(f"SELECT * FROM {table}")
                cols = [d[0] for d in cur.description]
                for row in cur:
                    f.write(json.dumps({"table": table, "data": dict(zip(cols, row))},
                                       ensure_ascii=False, default=str) + "\n")
                    count += 1
            except sqlite3.OperationalError:
                pass  # 表不存在时跳过
    conn.close()
    return count


def verify_backup(db_path: Path, jsonl_path: Path) -> bool:
    """恢复演练：在临时 DB 中导入 JSONL 后抽检。"""
    import tempfile
    tmp_db = Path(tempfile.mktemp(suffix=".db"))
    try:
        # 找 JSONL 中的 memories 数据重建
        conn = sqlite3.connect(str(tmp_db))
        count = 0
        with open(jsonl_path, encoding="utf-8") as f:
            for line in f:
                rec = json.loads(line)
                if rec.get("table") != "memories":
                    continue
                data = rec["data"]
                if count == 0:
                    cols = ", ".join(k for k in data.keys())
                    placeholders = ", ".join("?" for _ in data)
                    conn.execute(f"CREATE TABLE IF NOT EXISTS memories ({cols})")
                try:
                    conn.execute(
                        f"INSERT OR IGNORE INTO memories VALUES ({', '.join('?' for _ in data)})",
                        list(data.values()))
                    count += 1
                except Exception:
                    pass
        conn.commit()
        # 抽检
        cur = conn.execute("SELECT COUNT(*) FROM memories")
        total = cur.fetchone()[0]
        print(f"  恢复演练: {total} 条记忆（源 JSONL {count} 行）→ {'✓' if total > 0 else '✗ 空'}")
        conn.close()
        return total > 0
    finally:
        tmp_db.unlink(missing_ok=True)


def cleanup_old(backup_dir: Path, keep: int) -> int:
    """清理旧备份（保留最近 keep 份）。"""
    removed = 0
    for pattern in ("memoria_export_*.jsonl", "memoria_db_*.db"):
        files = sorted(backup_dir.glob(pattern), reverse=True)
        for old in files[keep:]:
            old.unlink()
            removed += 1
    return removed


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--verify", action="store_true", help="只做恢复演练")
    args = ap.parse_args()

    if not DB.is_file():
        print(f"✗ DB 不存在: {DB}")
        return 1

    now = datetime.now()
    date_str = now.strftime("%Y%m%d")

    if args.verify:
        jsonl = BACKUP_DIR / f"memoria_export_{date_str}.jsonl"
        if not jsonl.is_file():
            print(f"✗ 找不到今日导出: {jsonl}")
            return 1
        ok = verify_backup(DB, jsonl)
        return 0 if ok else 1

    BACKUP_DIR.mkdir(parents=True, exist_ok=True)

    # 1) WAL checkpoint
    checkpoint_wal(DB)
    print(f"✓ WAL checkpoint 完成")

    # 2) JSONL 导出
    jsonl_path = BACKUP_DIR / f"memoria_export_{date_str}.jsonl"
    count = export_jsonl(DB, jsonl_path)
    print(f"✓ JSONL 导出 {count} 行 → {jsonl_path.name}")

    # 3) DB 快照
    db_path = BACKUP_DIR / f"memoria_db_{date_str}.db"
    checkpoint_wal(DB)
    shutil.copy2(DB, db_path)
    print(f"✓ DB 快照 → {db_path.name} ({db_path.stat().st_size / 1048576:.0f} MB)")

    # 4) 清理旧备份
    removed = cleanup_old(BACKUP_DIR, KEEP_DAYS)
    if removed:
        print(f"✓ 清理 {removed} 份旧备份")

    # 5) 恢复演练
    ok = verify_backup(DB, jsonl_path)

    print(f"\n=== 备份完成 {now.strftime('%Y-%m-%d %H:%M')} ===")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
