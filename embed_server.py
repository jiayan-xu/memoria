#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Memoria 本地嵌入服务（语义检索后端）
====================================

为独立 MCP 服务（memoria-server）在**查询时**提供 query embedding，
使 HNSW 向量语义搜索真正生效。

背景（评测报告根因）：
    memoria-server 是纯 Rust 独立二进制，自身不持有 embedding 模型。
    原设计把 query 向量交给 Python 通过 `cache_query_vector()` 预缓存，
    但独立 HTTP 部署路径从不调用，导致 semantic_search 的 QueryCache 恒为空、
    HNSW 那 1174 个向量永不参与排序。本服务补齐这条链路：
        memory_search → POST /embed → 向量 → 注入 QueryCache → HNSW 参与融合。

模型：shibing624/text2vec-base-chinese（768 维，与 HNSW 存量向量同构）
依赖：sentence_transformers / transformers / torch（已装于系统 Python）
安全：仅监听 127.0.0.1（回环），不暴露外网。

启动：
    python embed_server.py
    MEMORIA_EMBED_PORT=8777 python embed_server.py

接口：
    POST /embed
        body:  {"texts": ["..."], "normalize": false}
        return: {"embeddings": [[...]], "dim": 768, "model": "shibing624/text2vec-base-chinese"}
    GET  /health
        return: {"status": "ok", "model": "...", "dim": 768}
"""

import os
import sys
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ── 强制离线 + CPU，避免 HuggingFace 不可达时卡死 ──
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
os.environ.setdefault("CUDA_VISIBLE_DEVICES", "")
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

MODEL_NAME = os.environ.get("MEMORIA_EMBED_MODEL", "shibing624/text2vec-base-chinese")
HOST = os.environ.get("MEMORIA_EMBED_HOST", "127.0.0.1")
PORT = int(os.environ.get("MEMORIA_EMBED_PORT", "8777"))

_model = None
_model_lock = threading.Lock()


def get_model():
    """懒加载并缓存模型（线程安全，仅加载一次）。"""
    global _model
    if _model is None:
        with _model_lock:
            if _model is None:
                from sentence_transformers import SentenceTransformer
                import torch

                torch.set_num_threads(max(1, (os.cpu_count() or 2) // 2))
                _model = SentenceTransformer(MODEL_NAME, device="cpu")
    return _model


def embed_texts(texts, normalize=False):
    """批量文本 → 向量列表（list[list[float]]）。"""
    model = get_model()
    vecs = model.encode(
        texts,
        normalize_embeddings=normalize,
        show_progress_bar=False,
        convert_to_numpy=True,
    )
    return [v.tolist() for v in vecs]


def _model_dim():
    try:
        return int(get_model().get_sentence_embedding_dimension())
    except Exception:
        return 768


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "memoria-embed/1.0"

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
            self._send(200, {"status": "ok", "model": MODEL_NAME, "dim": _model_dim()})
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
            self._send(200, {"embeddings": [], "dim": 0, "model": MODEL_NAME})
            return

        normalize = bool(req.get("normalize", False))
        try:
            embeddings = embed_texts(texts, normalize)
        except Exception as e:
            self._send(500, {"error": f"encode failed: {e}"})
            return

        dim = len(embeddings[0]) if embeddings else 0
        self._send(200, {"embeddings": embeddings, "dim": dim, "model": MODEL_NAME})

    def log_message(self, *args):
        pass  # 静默，避免刷屏


def main():
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"[embed] loading {MODEL_NAME} (offline mode)...", flush=True)
    dim = _model_dim()
    print(f"[embed] loaded {MODEL_NAME} ({dim}d) -> http://{HOST}:{PORT}/embed", flush=True)
    print("[embed] press Ctrl+C to stop.", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        print("[embed] stopped.", flush=True)


if __name__ == "__main__":
    main()
