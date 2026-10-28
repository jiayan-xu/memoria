#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""平台每日备份（R0-H1 重建 2026-10-13；E1 manifest 2026-10-19；E1② 多目标 2026-10-19）。

覆盖（按 D9 分级，2026-10-19 拍板）
----------------------------------
- memoria：memoria.db（必备）、audit.db（必备，周分区）
- agent-core：audit_events.db（必备）、checkpoints.db（必备，审批权威）、
  harness.db（建议，orchestration 真相源）、meta_evolution.db（可选）、
  agent_core_boot.db（**备份但不恢复**——启动熔断锁存，恢复=恢复故障态）
- dashboard：dashboard.db（必备）、event_bus.db（必备）、data/harness_store.db（建议）
- 排除：agent-core/data/checkpoints.db 0B、temp_run/harness.db、_diag/harness.db；
  dashboard/data/dashboard.db 与 data/solid_waste.db 均为 0B 空壳。

策略
----
- SQLite backup API 一致性热备（源库只读打开，WAL 随快照落盘）。
- 双份：C:\\services\\backups\\memoria + F:\\platform_backup\\memoria
  （F 不在位时跳过并警告，不拖垮本地备份）。
- 每份产出即验：PRAGMA quick_check + 目标表行数证据；必备库缺失即失败。
- 全部成功后写 manifest_<stamp>.json（sha256/size/source_mtime/DB user_version/
  role/restore/多系统版本戳）+ latest_manifest.json，供 `svc verify-backup` 校验。

用法
----
    python scripts/daily_memoria_backup.py          # 计划任务每日 04:30
    python scripts/daily_memoria_backup.py --dry-run
"""
import argparse
import glob
import hashlib
import json
import os
import sqlite3
import sys
import tempfile
import time
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
MEMORIA_DATA_DIR = os.environ.get(
    "MEMORIA_DATA_DIR",
    os.path.join(HERE, "..", "..", "memoria", "data"))
AGENT_CORE_DIR = r"C:\Users\user\agent-core"
DASHBOARD_DIR = r"C:\Users\user\dashboard"
LOCAL_DIR = r"C:\services\backups\memoria"
F_DIR = r"F:\platform_backup\memoria"
LOCAL_KEEP, F_KEEP = 14, 30
SCRIPT_VERSION = "3"
MEMORIA_EXE = r"C:\services\memoria-open\target\release\memoria-server.exe"
AGENT_CORE_EXE = os.path.join(AGENT_CORE_DIR, r"target\release\agent-core.exe")

# role: must=缺失即失败；recommended/optional/boot-latch=缺失仅告警
# restore: False 表示恢复流程必须跳过（boot 锁存）
# evidence: "memories" / "audit" / [表名...]
TARGETS = [
    {"name": "memoria.db", "src": os.path.join(MEMORIA_DATA_DIR, "memoria.db"),
     "system": "memoria", "role": "must", "restore": True, "evidence": "memories"},
    {"name": "audit.db", "src": os.path.join(MEMORIA_DATA_DIR, "audit.db"),
     "system": "memoria", "role": "must", "restore": True, "evidence": "audit"},
    {"name": "agent-core.audit_events.db",
     "src": os.path.join(AGENT_CORE_DIR, "audit_events.db"),
     "system": "agent-core", "role": "must", "restore": True,
     "evidence": ["audit_events"]},
    {"name": "agent-core.checkpoints.db",
     "src": os.path.join(AGENT_CORE_DIR, "checkpoints.db"),
     "system": "agent-core", "role": "must", "restore": True,
     "evidence": ["checkpoints", "approvals"]},
    {"name": "agent-core.harness.db",
     "src": os.path.join(AGENT_CORE_DIR, "harness.db"),
     "system": "agent-core", "role": "recommended", "restore": True,
     "evidence": ["orchestration_phase"]},
    {"name": "agent-core.meta_evolution.db",
     "src": os.path.join(AGENT_CORE_DIR, "meta_evolution.db"),
     "system": "agent-core", "role": "optional", "restore": True,
     "evidence": ["meta_kv"]},
    {"name": "agent-core.boot.db",
     "src": os.path.join(AGENT_CORE_DIR, "agent_core_boot.db"),
     "system": "agent-core", "role": "boot-latch", "restore": False,
     "evidence": ["boot_lifecycle"]},
    {"name": "dashboard.dashboard.db",
     "src": os.path.join(DASHBOARD_DIR, "dashboard.db"),
     "system": "dashboard", "role": "must", "restore": True,
     "evidence": ["vehicle_entrance", "video_files"]},
    {"name": "dashboard.event_bus.db",
     "src": os.path.join(DASHBOARD_DIR, "event_bus.db"),
     "system": "dashboard", "role": "must", "restore": True,
     "evidence": ["agent_events"]},
    {"name": "dashboard.harness_store.db",
     "src": os.path.join(DASHBOARD_DIR, "data", "harness_store.db"),
     "system": "dashboard", "role": "recommended", "restore": True,
     "evidence": ["harnesses"]},
]


def log(msg):
    line = f"[backup {datetime.now().strftime('%m-%d %H:%M:%S')}] {msg}"
    print(line, flush=True)
    # pythonw（计划任务静默模式）下 stdout 丢失——双写文件日志保底
    try:
        with open(os.path.join(LOCAL_DIR, "backup.log"), "a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        pass


def _count(con, table):
    q = '"' + table.replace('"', '""') + '"'
    try:
        return con.execute("SELECT COUNT(*) FROM " + q).fetchone()[0]
    except sqlite3.Error:
        return None


def _row_evidence(con, evidence):
    """行数证据：memories / audit 周分区 / 指定表清单。"""
    tables = {r[0] for r in con.execute(
        "SELECT name FROM sqlite_master WHERE type='table'")}
    if evidence == "memories":
        return "rows=" + str(_count(con, "memories"))
    if evidence == "audit":
        total = _count(con, "audit_log") or 0
        parts = sorted(t for t in tables if t.startswith("audit_log_"))
        for t in parts:
            total += _count(con, t) or 0
        return f"rows={total} (audit partitions={len(parts)})"
    if isinstance(evidence, list):
        parts = []
        for t in evidence:
            n = _count(con, t)
            parts.append(f"{t}={n if n is not None else 'n/a'}")
        return "rows: " + " ".join(parts)
    return "rows=n/a"


def snapshot(src_path: str, dst_path: str, evidence) -> tuple:
    """SQLite backup API 热备，返回 (ok, detail)。SQL 全部字面量。"""
    try:
        src = sqlite3.connect(f"file:{src_path}?mode=ro", uri=True, timeout=60)
        dst = sqlite3.connect(dst_path)
        with dst:
            src.backup(dst)
        dst.close()
        chk = sqlite3.connect(f"file:{dst_path}?mode=ro", uri=True, timeout=30)
        verdict = chk.execute("PRAGMA quick_check").fetchone()[0]
        detail = f"quick_check={verdict} {_row_evidence(chk, evidence)}"
        chk.close()
        src.close()
        return (verdict == "ok", detail)
    except Exception as e:  # noqa: BLE001 — 备份失败必须显式暴露
        return (False, f"{type(e).__name__}: {e}")


def db_user_version(path: str) -> int:
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=30)
        v = con.execute("PRAGMA user_version").fetchone()[0]
        con.close()
        return int(v)
    except Exception:  # noqa: BLE001
        return -1


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def file_info(path: str) -> dict:
    try:
        st = os.stat(path)
        return {
            "path": path,
            "mtime": datetime.fromtimestamp(st.st_mtime).isoformat(timespec="seconds"),
            "size": st.st_size,
        }
    except OSError:
        return {"path": path, "mtime": None, "size": None}


def source_versions() -> dict:
    return {
        "memoria": file_info(MEMORIA_EXE),
        "agent-core": file_info(AGENT_CORE_EXE),
        "dashboard": file_info(os.path.join(DASHBOARD_DIR, "main.py")),
    }


def _short(detail, limit=240):
    lines = str(detail).splitlines() or [str(detail)]
    return lines[0][:limit] + (f" ... (+{len(lines)-1} lines)" if len(lines) > 1 else "")


def _atomic_json(path: str, data: dict):
    directory = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(prefix=".manifest-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            try:
                os.remove(tmp)
            except OSError:
                pass


def write_manifest(directory: str, stamp: str, entries: list,
                   corrupt: list, required: list) -> str:
    man = {
        "version": 1,
        "stamp": stamp,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "tool": "daily_memoria_backup",
        "script_version": SCRIPT_VERSION,
        "destination": directory,
        "required": required,
        "corrupt": corrupt,
        "source_versions": source_versions(),
        "files": entries,
    }
    path = os.path.join(directory, f"manifest_{stamp}.json")
    _atomic_json(path, man)
    _atomic_json(os.path.join(directory, "latest_manifest.json"), man)
    return path


def roll(directory: str, keep: int):
    """按 mtime 滚动清理旧备份（只清本脚本命名规则的文件）。"""
    for t in TARGETS:
        files = sorted(glob.glob(os.path.join(directory, t["name"] + ".*_*.db")),
                       key=os.path.getmtime, reverse=True)
        for f in files[keep:]:
            try:
                os.remove(f)
                for side in (f + "-shm", f + "-wal"):
                    if os.path.exists(side):
                        os.remove(side)
                log(f"滚动清理 {f}")
            except OSError:
                pass
    mans = sorted(glob.glob(os.path.join(directory, "manifest_2*.json")),
                  key=os.path.getmtime, reverse=True)
    for f in mans[keep:]:
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
    corrupt = []
    corrupt_names = set()
    required = [t["name"] for t in TARGETS if t["role"] == "must"]
    entries = {}
    for d in (LOCAL_DIR, F_DIR):
        if not os.path.isdir(d):
            if d.startswith("F:"):
                log(f"⚠ F 备份目录不可用（{d}），跳过外盘副本")
                continue
            os.makedirs(d, exist_ok=True)
        entries[d] = []

    for t in TARGETS:
        src = t["src"]
        if not os.path.exists(src):
            log(f"⚠ 源不存在（{t['role']}）: {src}")
            if t["role"] == "must":
                failures.append(f"{t['name']}@source")
            continue
        uv = db_user_version(src)
        src_mtime = datetime.fromtimestamp(os.path.getmtime(src)).isoformat(timespec="seconds")
        for d, keep in ((LOCAL_DIR, LOCAL_KEEP), (F_DIR, F_KEEP)):
            if d.startswith("F:") and not os.path.isdir(d):
                continue
            dst = os.path.join(d, t["name"] + "." + stamp + ".db")
            if args.dry_run:
                log(f"[dry-run] {src} -> {dst}")
                continue
            t0 = time.time()
            ok, detail = snapshot(src, dst, t["evidence"])
            size = os.path.getsize(dst) if os.path.exists(dst) else 0
            if ok:
                log(f"✅ {t['name']} -> {d} ({size / 2**20:.0f}MB, "
                    f"{time.time()-t0:.0f}s, {detail})")
                try:
                    entries[d].append({
                        "name": os.path.basename(dst),
                        "system": t["system"],
                        "role": t["role"],
                        "restore": t["restore"],
                        "size": size,
                        "sha256": sha256_file(dst),
                        "source": src,
                        "source_mtime": src_mtime,
                        "user_version": uv,
                        "check": detail,
                    })
                except Exception as e:  # noqa: BLE001
                    log(f"❌ manifest 条目生成失败 {dst}: {e}")
                    failures.append(f"{t['name']}@{d}:hash")
            else:
                short = _short(detail)
                # 失败产物不留在备份目录（损坏副本会被误当可用备份）
                for side in (dst, dst + "-shm", dst + "-wal"):
                    if os.path.exists(side):
                        try:
                            os.remove(side)
                        except OSError:
                            pass
                if t["role"] == "must":
                    log(f"❌ {t['name']} -> {d} 失败: {short}")
                    failures.append(f"{t['name']}@{d}")
                else:
                    log(f"⚠ {t['name']} 损坏/不可备（{t['role']}，已隔离，不阻塞）: {short}")
                    if t["name"] not in corrupt_names:
                        corrupt_names.add(t["name"])
                        corrupt.append({
                            "name": t["name"],
                            "system": t["system"],
                            "role": t["role"],
                            "source": src,
                            "error": short,
                        })

    if not args.dry_run and not failures:
        for d in (LOCAL_DIR, F_DIR):
            if entries.get(d):
                try:
                    p = write_manifest(d, stamp, entries[d], corrupt, required)
                    log(f"🧾 manifest -> {p} ({len(entries[d])} files, "
                        f"{len(corrupt)} corrupt)")
                except Exception as e:  # noqa: BLE001
                    log(f"❌ manifest 写入失败 {d}: {e}")
                    failures.append(f"manifest@{d}")
        roll(LOCAL_DIR, LOCAL_KEEP)
        roll(F_DIR, F_KEEP)
    if failures:
        log(f"备份存在失败项: {failures}")
        sys.exit(1)
    if corrupt:
        log("⚠ 以下非必备库损坏/不可备，已隔离（详见 manifest.corrupt）: "
            + ", ".join(c["name"] for c in corrupt))
    log("全部完成")


if __name__ == "__main__":
    main()
