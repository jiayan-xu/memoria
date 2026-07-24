#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
一条龙修复流水线（2026-07-24）
================================
背景：1024 重嵌事故——SiliconFlow Qwen3-VL-Embedding-8B 的 data[].index
在 batch>8 时只返回 0..7（重复），而 cloud_backfill 旧版 BATCH=32 按 index
映射 + 静默零填充，导致 42333 条里 31749 条（75%）被写成全零向量，语义索引
75% 损坏。本脚本修复：

  STEP1  cloud_backfill.py --only-zero --backup --dim 1024
         （只重嵌 memory_vectors 中零向量行；BATCH=8 绕开 index bug；
          失败槽跳过绝不写零向量；重嵌前备份 memory_vectors 表）
  STEP2  杀掉旧 memoria → 删 vector_index/*.bin → 用 logged launcher 重启
         → 从已修复的 memory_vectors 全表重建 HNSW
  STEP3  跑 eval_nl_recall.py 出真实 A/B（覆盖 nl_recall_result.json）

用法：python reembed_zero_pipeline.py   （建议后台运行，约 25–45 分钟）
"""
import os
import sys
import json
import time
import glob
import subprocess
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
QCLAW = os.path.expanduser("~/.qclaw/workspace")
MEMORIA_DIR = os.path.join(QCLAW, "memoria")
DB_PATH = os.path.join(MEMORIA_DIR, "data", "memoria.db")
SF_CANDIDATES = [
    os.path.expanduser("~/agent-core/.env"),
    os.path.join(MEMORIA_DIR, ".env"),
]


def load_env_file(p):
    d = {}
    try:
        for line in open(p, encoding="utf-8-sig"):
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            d[k.strip()] = v.strip().strip('"').strip("'")
    except OSError:
        pass
    return d


def get_sf_key():
    if os.environ.get("SILICONFLOW_API_KEY"):
        return os.environ["SILICONFLOW_API_KEY"]
    for p in SF_CANDIDATES:
        d = load_env_file(p)
        if d.get("SILICONFLOW_API_KEY"):
            return d["SILICONFLOW_API_KEY"]
    return ""


def run(cmd, env=None, cwd=None):
    print(">>", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, env=env, cwd=cwd)
    if r.returncode != 0:
        raise RuntimeError(f"cmd failed {r.returncode}: {cmd}")
    return r


def find_memoria_pids():
    try:
        out = subprocess.check_output(
            ["powershell", "-NoProfile", "-Command",
             "(Get-NetTCPConnection -LocalPort 9003 -State Listen -ErrorAction SilentlyContinue).OwningProcess"],
            text=True, stderr=subprocess.DEVNULL)
        return [x.strip() for x in out.split() if x.strip()]
    except Exception:
        return []


def kill_memoria():
    pids = find_memoria_pids()
    if pids:
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        f"Stop-Process -Id {','.join(pids)} -Force -ErrorAction SilentlyContinue"],
                       check=False)
        time.sleep(3)
    rem = find_memoria_pids()
    print("[ok] memoria killed" if not rem else f"[warn] memoria still alive: {rem}", flush=True)


def restart_memoria():
    vi = os.path.join(MEMORIA_DIR, "data", "vector_index")
    for f in glob.glob(os.path.join(vi, "*.bin")):
        os.remove(f)
        print("[del]", os.path.basename(f), flush=True)
    launcher = os.path.join(HERE, "start_memoria_logged.py")
    # launcher 内部 load_env + Popen memoria-server.exe
    subprocess.Popen([sys.executable, launcher], cwd=ROOT, start_new_session=True)
    ok = False
    for i in range(180):
        try:
            with urllib.request.urlopen("http://127.0.0.1:9003/health", timeout=3) as r:
                if b'"status":"ok"' in r.read():
                    print(f"[ok] memoria healthy after ~{i}s", flush=True)
                    ok = True
                    break
        except Exception:
            pass
        time.sleep(1)
    if not ok:
        print("[warn] memoria health timeout", flush=True)


def main():
    key = get_sf_key()
    if not key:
        print("[FATAL] no SILICONFLOW_API_KEY found", file=sys.stderr)
        sys.exit(1)
    print(f"[info] SF key loaded (len={len(key)})", flush=True)
    env = os.environ.copy()
    env["SILICONFLOW_API_KEY"] = key

    print("=== STEP1: reembed zero vectors (--only-zero --backup --dim 1024) ===", flush=True)
    run([sys.executable, os.path.join(HERE, "cloud_backfill.py"),
         "--only-zero", "--backup", "--dim", "1024",
         "--model", "Qwen/Qwen3-VL-Embedding-8B", "--db-path", DB_PATH],
        env=env, cwd=ROOT)

    print("=== STEP2: restart memoria (rebuild HNSW from fixed vectors) ===", flush=True)
    kill_memoria()
    restart_memoria()

    print("=== STEP3: eval NL recall A/B ===", flush=True)
    run([sys.executable, os.path.join(HERE, "eval_nl_recall.py")], cwd=ROOT)

    res = os.path.join(HERE, "nl_recall_result.json")
    if os.path.exists(res):
        d = json.load(open(res, encoding="utf-8"))
        print("=== RESULT ===", flush=True)
        print(json.dumps(d, ensure_ascii=False, indent=2), flush=True)
    print("=== PIPELINE DONE ===", flush=True)


if __name__ == "__main__":
    main()
