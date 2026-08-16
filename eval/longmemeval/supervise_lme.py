#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""LongMemEval 自愈监督器：监控评测进度，服务端 MCP 卡死时自动重启

触发条件（同时满足）：
- 结果文件超过 STALL_SEC（默认 900s = 15 分钟）无新条目；
- MCP 探活（memory_search_v2，25s 超时）连续 FAIL_PROBE 次失败。
→ 判定服务端挂起：kill 评测进程 → 重启 memoria-server → 等 health → 重启评测（resume）。

阈值说明（2026-08-16 事故后校准）：
- 原 STALL_SEC=240 会把「单题灌库（数百块 × SiliconFlow 嵌入）+ LLM 作答」这类
  慢但正常的题目误判为挂起（实测单题可 >4 分钟），进而 taskkill 健康服务端，
  引发重启死循环（1GB 库 HNSW 重建需 ~10 分钟，而原重启等待仅 95s）。故：
  STALL_SEC=900、探活超时 25s、重启等待最多 15 分钟。
正常慢问题（MCP 仍响应）不满足条件，不会误杀。
"""
import json
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
RESULTS = HERE / "longmemeval_results.json"
EVAL_SCRIPT = HERE / "longmemeval_eval.py"
# 运行时镜像的启动脚本（含 .env 注入与隐藏启动）；环境变量可覆盖
LAUNCHER = os.environ.get(
    "MEMORIA_LAUNCHER",
    str(Path.home() / ".qclaw" / "workspace" / "memoria" / "start_memoria_only.ps1"),
)
# 评测数据默认取运行时镜像（277MB，不入库）；环境变量可覆盖
DATA_PATH = os.environ.get(
    "LME_DATA_PATH",
    str(Path.home() / ".qclaw" / "workspace" / "memoria" / "eval" / "longmemeval" / "longmemeval_s_cleaned.json"),
)
LOG = HERE / "supervised_run.log"
LOG_ERR = HERE / "supervised_run.err"

STALL_SEC = int(os.environ.get("LME_STALL_SEC", "900"))   # 15 分钟无进展视为可疑
FAIL_PROBE = 2                                            # 连续 2 次探活失败才动手
TARGET = 500

ENV = os.environ.copy()
ENV["LME_DATA_PATH"] = DATA_PATH
ENV["LME_CHUNK_CHARS"] = "5000"
ENV["LME_TOP_K"] = "8"


def count_done():
    try:
        with open(RESULTS, encoding="utf-8") as f:
            return len(json.load(f))
    except Exception:
        return -1


def mcp_alive() -> bool:
    """MCP 探活：用真实检索探针（搜索路径才是会挂的；db_stats 在挂起时仍能通过）"""
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "memory_search_v2",
                                  "arguments": {"query": "probe", "namespace": "nonexistent_ns_probe",
                                                "max_results": 3}}}).encode()
    key = ""
    for line in open(os.path.join(os.path.expanduser("~"), "agent-core", ".env"), encoding="utf-8"):
        if line.startswith("MEMORIA_JARVIS_BADGE="):
            key = line.strip().split("=", 1)[1].strip().strip('"').strip("'")
            break
    req = urllib.request.Request(
        "http://127.0.0.1:9003/mcp", data=body,
        headers={"Content-Type": "application/json",
                 "X-Agent-Id": "jarvis", "X-Agent-Key": key})
    try:
        urllib.request.urlopen(req, timeout=25).read()
        return True
    except Exception:
        return False


def health_ok() -> bool:
    try:
        r = json.loads(urllib.request.urlopen("http://127.0.0.1:9003/health", timeout=5).read().decode())
        return r.get("status") == "ok"
    except Exception:
        return False


def restart_server():
    subprocess.run(["taskkill", "/f", "/im", "memoria-server.exe"],
                   capture_output=True, text=True)
    time.sleep(3)
    subprocess.Popen(["powershell", "-ExecutionPolicy", "Bypass", "-File", LAUNCHER],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # 1GB 库 + ~20 万向量：HNSW 启动重建可达 10 分钟，等待放宽到 15 分钟
    for _ in range(90):
        time.sleep(10)
        if health_ok():
            time.sleep(5)  # 给 HNSW 重建/预热一点时间
            return True
    return False


def main():
    print(f"supervisor started: stall={STALL_SEC}s target={TARGET}", flush=True)
    eval_proc = None
    last_count = count_done()
    last_change = time.time()
    fail_streak = 0

    while last_count < TARGET:
        c = count_done()
        now = time.time()
        if c > last_count:
            last_count = c
            last_change = now
            fail_streak = 0
            print(f"[{time.strftime('%H:%M:%S')}] progress={c}/{TARGET}", flush=True)

        # 评测进程保活
        if eval_proc is None or eval_proc.poll() is not None:
            print(f"[{time.strftime('%H:%M:%S')}] (re)starting eval", flush=True)
            with open(LOG, "a", encoding="utf-8") as out, open(LOG_ERR, "a", encoding="utf-8") as err:
                eval_proc = subprocess.Popen([sys.executable, EVAL_SCRIPT, "--ingest-workers", "4"],
                                             env=ENV, stdout=out, stderr=err)
            last_change = now
            fail_streak = 0

        stalled = (now - last_change) > STALL_SEC
        if stalled:
            if mcp_alive():
                print(f"[{time.strftime('%H:%M:%S')}] 慢问题（MCP 活，无进展 {int(now-last_change)}s）——继续等", flush=True)
                fail_streak = 0
                last_change = now  # 重置，避免每 30s 重复打印
            else:
                fail_streak += 1
                print(f"[{time.strftime('%H:%M:%S')}] MCP 探活失败 {fail_streak}/{FAIL_PROBE}（stalled {int(now-last_change)}s）", flush=True)
                if fail_streak >= FAIL_PROBE:
                    print(f"[{time.strftime('%H:%M:%S')}] 判定服务端挂起 → 重启服务端 + 评测", flush=True)
                    if eval_proc is not None and eval_proc.poll() is None:
                        eval_proc.kill()
                    ok = restart_server()
                    print(f"[{time.strftime('%H:%M:%S')}] server restart {'ok' if ok else 'FAILED'}", flush=True)
                    last_change = time.time()
                    fail_streak = 0
        time.sleep(30)

    print(f"TARGET REACHED: {last_count}/{TARGET}", flush=True)
    if eval_proc is not None and eval_proc.poll() is None:
        eval_proc.wait(timeout=600)
    print("supervisor exit", flush=True)


if __name__ == "__main__":
    main()
