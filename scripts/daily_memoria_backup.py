#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""memoria 每日备份（R0-H1 重建，2026-10-13）——替代已死的 MemoriaCloudBackup。

背景：旧 MemoriaCloudBackup 任务指向 .qclaw 死目录脚本，2026-08-23 起停摆，
主库（~675MB）裸奔 47 天后于 10-13 重建本链路。

策略
----
- 用 SQLite backup API 做**一致性热备**（源库以只读打开，无需停写，WAL 内容
  随快照落盘），不用裸文件拷贝——断电/写半场景下快照仍一致。
- 双份：本地 C:\\services\\backups\\memoria（滚动 14 份）+ F:\\platform_backup\\memoria
  （滚动 30 份；F 不在位时跳过并警告，不让外盘缺失拖垮本地备份）。
- 备 memoria.db（主库）与 audit.db（注册/审计，小）。
- 每份产出即验：PRAGMA quick_check + 行数抽查，失败即退出码 1。

用法
----
    python scripts/daily_memoria_backup.py          # 计划任务每日 04:30 调用
    python scripts/daily_memoria_backup.py --dry-run
"""
import argparse
import glob
import os
import sqlite3
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.environ.get(
    "MEMORIA_DATA_DIR",
    os.path.join(HERE, "..", "..", "memoria", "data"))
LOCAL_DIR = r"C:\services\backups\memoria"
F_DIR = r"F:\platform_backup\memoria"
LOCAL_KEEP, F_KEEP = 14, 30
DBS = ("memoria.db", "audit.db")


def log(msg):
    line = f"[backup {datetime.now().strftime('%m-%d %H:%M:%S')}] {msg}"
    print(line, flush=True)
    # pythonw（计划任务静默模式）下 stdout 丢失——双写文件日志保底
    try:
        with open(os.path.join(LOCAL_DIR, "backup.log"), "a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass


def snapshot(src_path: str, dst_path: str) -> tuple:
    """SQLite backup API 热备，返回 (ok, detail)。SQL 全部字面量。"""
    try:
        src = sqlite3.connect(f"file:{src_path}?mode=ro", uri=True, timeout=60)
        dst = sqlite3.connect(dst_path)
        with dst:
            src.backup(dst)
        dst.close()
        chk = sqlite3.connect(f"file:{dst_path}?mode=ro", uri=True, timeout=30)
        verdict = chk.execute("PRAGMA quick_check").fetchone()[0]
        evidence = "rows=n/a"
        try:
            evidence = "rows=" + str(chk.execute("SELECT COUNT(*) FROM memories").fetchone()[0])
        except sqlite3.OperationalError:
            try:
                evidence = "rows=" + str(chk.execute("SELECT COUNT(*) FROM audit_log").fetchone()[0])
            except sqlite3.OperationalError:
                pass
        chk.close()
        src.close()
        return (verdict == "ok", f"quick_check={verdict} {evidence}")
    except Exception as e:  # noqa: BLE001 — 备份失败必须显式暴露
        return (False, f"{type(e).__name__}: {e}")


def roll(directory: str, keep: int):
    """按 mtime 滚动清理旧备份（只清本脚本命名规则的文件）。"""
    for db in DBS:
        files = sorted(glob.glob(os.path.join(directory, db + ".*_*.db")),
                       key=os.path.getmtime, reverse=True)
        for f in files[keep:]:
            try:
                os.remove(f)
                log(f"滚动清理 {f}")
            except OSError:
                pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    stamp = datetime.now().strftime("%Y%m%d_%H%M")
    failures = []
    for d in (LOCAL_DIR, F_DIR):
        if not os.path.isdir(d):
            if d.startswith("F:"):
                log(f"⚠ F 备份目录不可用（{d}），跳过外盘副本")
                continue
            os.makedirs(d, exist_ok=True)

    for db in DBS:
        src = os.path.join(DATA_DIR, db)
        if not os.path.exists(src):
            log(f"⚠ 源不存在，跳过: {src}")
            continue
        for d, keep in ((LOCAL_DIR, LOCAL_KEEP), (F_DIR, F_KEEP)):
            if d.startswith("F:") and not os.path.isdir(d):
                continue
            dst = os.path.join(d, db + "." + stamp + ".db")
            if args.dry_run:
                log(f"[dry-run] {src} -> {dst}")
                continue
            t0 = time.time()
            ok, detail = snapshot(src, dst)
            size_mb = os.path.getsize(dst) / 2**20 if os.path.exists(dst) else 0
            if ok:
                log(f"✅ {db} -> {d} ({size_mb:.0f}MB, {time.time()-t0:.0f}s, {detail})")
            else:
                log(f"❌ {db} -> {dst} 失败: {detail}")
                failures.append(f"{db}@{d}")
    if not args.dry_run and not failures:
        roll(LOCAL_DIR, LOCAL_KEEP)
        roll(F_DIR, F_KEEP)
    if failures:
        log(f"备份存在失败项: {failures}")
        sys.exit(1)
    log("全部完成")


if __name__ == "__main__":
    main()
