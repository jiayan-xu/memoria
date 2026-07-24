#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Logged memoria launcher — identical env injection to start_memoria_only.py,
but captures stdout+stderr to eval/memoria_restart.log so we can read the
authoritative "[Memoria] HNSW vectors: N" line that proves the in-process
HNSW was rebuilt from the (now populated) memory_vectors table.
"""
import subprocess, os

QCLAW = os.path.expanduser("~/.qclaw/workspace")
MEMORIA_DIR = os.path.join(QCLAW, "memoria")
MEMORIA_BIN = os.path.join(QCLAW, "memoria-open")
LOG = os.path.join(QCLAW, "memoria-open", "eval", "memoria_restart.log")


def load_env(p):
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


menv = load_env(os.path.join(MEMORIA_DIR, ".env"))
env = {
    **os.environ,
    "MEMORIA_DB_PATH": os.path.join(MEMORIA_DIR, "data", "memoria.db"),
    "MEMORIA_BACKUP_DIR": os.path.join(MEMORIA_DIR, "data", "backups"),
    "MEMORIA_WEB_DIR": os.path.join(MEMORIA_DIR, "web"),
    "MEMORIA_ADMIN_KEY": menv.get("MEMORIA_ADMIN_KEY", ""),
    "MEMORIA_EMBEDDING_URL": menv.get("MEMORIA_EMBEDDING_URL", "http://127.0.0.1:8777/embed"),
    "MEMORIA_JARVIS_BADGE": menv.get("MEMORIA_JARVIS_BADGE", ""),
    "WATCH_DIRS": os.path.join(os.environ.get("APPDATA", ""), "reasonix", "sessions"),
}
logf = open(LOG, "w", encoding="utf-8")
subprocess.Popen(
    [os.path.join(MEMORIA_BIN, "target", "release", "memoria-server.exe")],
    cwd=MEMORIA_DIR,
    stdout=logf,
    stderr=subprocess.STDOUT,
    start_new_session=True,
    env=env,
)
print("memoria launched (logged) ->", LOG)
