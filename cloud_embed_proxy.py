#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Memoria 云端嵌入代理（SiliconFlow / OpenAI 兼容 /v1/embeddings）
=============================================================

把 memoria 的本地嵌入协议（POST /embed，body={"texts":[...]}，
返回 {"embeddings":[[...]]}）适配到云端大模型厂商。
当前默认对接 SiliconFlow 的 BAAI/bge-M3（1024 维，多语言 + 代码友好）。

为什么需要它：
    memoria-server（Rust）在查询时调用 MEMORIA_EMBEDDING_URL 拿 query 向量，
    契约是 {"texts":[...]} -> {"embeddings":[[...]],"dim":N}。
    云端厂商（SiliconFlow）走的是 OpenAI 格式
        POST /v1/embeddings  body={"model":...,"input":[...]}
        返回 {"data":[{"index":i,"embedding":[...]}]}
    本代理做协议翻译，使「换云端强模型」零改 Rust 代码。

重要前提（换模型的代价，不是本文件能消除的）：
    1) 维度变化：本地 text2vec-base-chinese=768d，bge-M3=1024d。
       HNSW 索引里存的记忆向量是固定维度的，query 维度(1024) 必须与
       记忆向量维度一致，否则余弦不可比、检索直接失效。
       => 必须「一次性全量重嵌」约 4 万条记忆并重建 HNSW。这是结构性成本，
          与本地/云端无关，只与「换模型」有关。
    2) 本代理只生成 query 向量；记忆向量由 memoria 自身在写入/重嵌时用
       同一模型生成（需 memoria 侧也走云端或对齐模型）。

环境变量：
    SILICONFLOW_API_KEY   必填，云端密钥（放 memoria/.env，gitignored）
    SILICONFLOW_MODEL     默认 BAAI/bge-M3
    SILICONFLOW_EMBED_URL 默认 https://api.siliconflow.cn/v1/embeddings
    CLOUD_EMBED_HOST      默认 127.0.0.1
    CLOUD_EMBED_PORT      默认 8778（与本地 8777 错开，可并存）
    CLOUD_EMBED_DIM       默认 1024（仅用于 /health 展示，实际以返回长度为准）

启动：
    C:\Python314\pythonw.exe cloud_embed_proxy.py
然后：
    memoria/.env 设 MEMORIA_EMBEDDING_URL=http://127.0.0.1:8778/embed
    重启 memoria 即可（注意：首次需全量重嵌，见上）

接口（与 embed_server.py 完全同构，memoria 无感）：
    POST /embed
        body:  {"texts": ["..."], "normalize": false}
        return: {"embeddings": [[...]], "dim": 1024, "model": "BAAI/bge-M3"}
    GET  /health
        return: {"status": "ok", "model": "...", "dim": 1024}
"""

import os
import sys
import json
import math
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

API_KEY = os.environ.get("SILICONFLOW_API_KEY", "").strip()
MODEL = os.environ.get("SILICONFLOW_MODEL", "BAAI/bge-M3")
EMBED_URL = os.environ.get("SILICONFLOW_EMBED_URL", "https://api.siliconflow.cn/v1/embeddings")
HOST = os.environ.get("CLOUD_EMBED_HOST", "127.0.0.1")
PORT = int(os.environ.get("CLOUD_EMBED_PORT", "8778"))
ADVERTISED_DIM = int(os.environ.get("CLOUD_EMBED_DIM", "1024"))

# 单次请求最多文本条数（防止超长 batch 触发厂商限流）
MAX_BATCH = int(os.environ.get("CLOUD_EMBED_MAX_BATCH", "32"))
HTTP_TIMEOUT = float(os.environ.get("CLOUD_EMBED_TIMEOUT", "20"))


def _l2_normalize(vec):
    norm = math.sqrt(sum(x * x for x in vec))
    if norm == 0:
        return vec
    return [x / norm for x in vec]


def embed_via_cloud(texts, normalize=False):
    """调云端 /v1/embeddings，返回 list[list[float]]，顺序与输入一致。

    抛异常由调用方捕获；本函数不吞错，便于上层返回 502 让 memoria 优雅降级。
    """
    if not API_KEY:
        raise RuntimeError("SILICONFLOW_API_KEY 未设置")
    payload = json.dumps(
        {"model": MODEL, "input": texts, "encoding_format": "float"}
    ).encode("utf-8")
    req = urllib.request.Request(
        EMBED_URL,
        data=payload,
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
        raw = resp.read().decode("utf-8")
    d = json.loads(raw)
    items = d.get("data")
    if not isinstance(items, list) or not items:
        # SiliconFlow 错误体： {"code":..., "message":..., "data":null}
        raise RuntimeError(f"云端返回异常: {raw[:300]}")
    # 按 index 排序还原输入顺序
    items_sorted = sorted(items, key=lambda it: it.get("index", 0))
    vecs = [list(it["embedding"]) for it in items_sorted]
    if normalize:
        vecs = [_l2_normalize(v) for v in vecs]
    return vecs


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "memoria-cloud-embed/1.0"

    def _send(self, code, payload):
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.split("?")[0] in ("/health", "/"):
            if not API_KEY:
                self._send(200, {"status": "warn", "model": MODEL, "dim": ADVERTISED_DIM,
                                 "message": "SILICONFLOW_API_KEY 未设置"})
            else:
                self._send(200, {"status": "ok", "model": MODEL, "dim": ADVERTISED_DIM})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path.split("?")[0] != "/embed":
            self._send(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
            raw = self.rfile.read(length) if length > 0 else b"{}"
            req = json.loads(raw.decode("utf-8") or "{}")
        except Exception as e:
            self._send(400, {"error": f"bad request: {e}"})
            return

        texts = req.get("texts")
        if not isinstance(texts, list) or not all(isinstance(t, str) for t in texts):
            self._send(400, {"error": "`texts` must be a list of strings"})
            return
        if not texts:
            self._send(200, {"embeddings": [], "dim": 0, "model": MODEL})
            return

        normalize = bool(req.get("normalize", False))
        # 超过单批上限则分批，再拼回
        all_vecs = []
        try:
            for i in range(0, len(texts), MAX_BATCH):
                chunk = texts[i:i + MAX_BATCH]
                all_vecs.extend(embed_via_cloud(chunk, normalize))
        except Exception as e:
            # 上游失败 -> 502，让 memoria 的 embed_query 优雅降级为 FTS/时间信号
            self._send(502, {"error": f"cloud embed failed: {e}"})
            return

        dim = len(all_vecs[0]) if all_vecs else 0
        self._send(200, {"embeddings": all_vecs, "dim": dim, "model": MODEL})

    def log_message(self, *args):
        pass  # 静默


def main():
    if not API_KEY:
        print("[cloud-embed] 警告: SILICONFLOW_API_KEY 未设置，/health 将返回 warn，/embed 将 502",
              flush=True)
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"[cloud-embed] {MODEL} -> http://{HOST}:{PORT}/embed (云端: {EMBED_URL})", flush=True)
    print(f"[cloud-embed] 使用环境变量 SILICONFLOW_API_KEY 提供密钥；重启 memoria "
          f"前设 MEMORIA_EMBEDDING_URL=http://{HOST}:{PORT}/embed", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        print("[cloud-embed] stopped.", flush=True)


if __name__ == "__main__":
    main()
