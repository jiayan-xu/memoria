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

支持两种后端（env 切换，互不影响）：
  - provider=local（默认）：sentence_transformers 离线 CPU 跑 text2vec（768d），
    与历史 HNSW 存量向量同构。
  - provider=siliconflow：调用 SiliconFlow 云端 /v1/embeddings（OpenAI-compatible），
    支持 MRL `dimensions` 参数，可输出 64~4096 任意维度。无需本地 GPU。
    模型默认 Qwen/Qwen3-VL-Embedding-8B（纯文本+多模态；纯文本库用 768 截断即可）。

安全：仅监听 127.0.0.1（回环），不暴露外网（云端调用走 HTTPS 出网，不监听外网）。

启动：
    MEMORIA_EMBED_PROVIDER=local python embed_server.py
    MEMORIA_EMBED_PROVIDER=siliconflow SILICONFLOW_API_KEY=sk-xxx python embed_server.py

接口：
    POST /embed
        body:  {"texts": ["..."], "normalize": false}
        return: {"embeddings": [[...]], "dim": 768, "model": "..."}
    GET  /health
        return: {"status": "ok", "model": "...", "dim": 768}
"""

import os
import sys
import json
import time
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ── 确保用户级 site-packages 在 sys.path（local 兜底路径需要 sentence_transformers）──
try:
    import site as _site
    _us = getattr(_site, "getusersitepackages", lambda: [])()
    if isinstance(_us, str):
        _us = [_us]
    for _p in _us:
        if _p and _p not in sys.path:
            sys.path.insert(0, _p)
except Exception:
    pass
try:
    _fb = os.path.expanduser(r"~\AppData\Roaming\Python\Python314\site-packages")
    if _fb and os.path.isdir(_fb) and _fb not in sys.path:
        sys.path.insert(0, _fb)
except Exception:
    pass

# ── 后端选择 ──
PROVIDER = os.environ.get("MEMORIA_EMBED_PROVIDER", "local").lower()
LOCAL_MODEL = os.environ.get("MEMORIA_EMBED_MODEL_LOCAL", "shibing624/text2vec-base-chinese")
SF_MODEL = os.environ.get("MEMORIA_EMBED_MODEL", "Qwen/Qwen3-VL-Embedding-8B")
SF_API = os.environ.get("SILICONFLOW_API_URL", "https://api.siliconflow.cn/v1/embeddings")
SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "")
# 输出维度：默认 768（与现有 hnsw.rs:DIM 对齐，零 Rust 改动）；
# 可选 1024/4096（需同步改 hnsw.rs:DIM 并重编）。MRL 截断，精度损失极小。
EMBED_DIM = int(os.environ.get("MEMORIA_EMBED_DIM", "768"))

HOST = os.environ.get("MEMORIA_EMBED_HOST", "127.0.0.1")
PORT = int(os.environ.get("MEMORIA_EMBED_PORT", "8777"))

# 云端出网时绕过本机代理（若有），避免请求被代理拦截
if "NO_PROXY" in os.environ:
    if "siliconflow" not in os.environ["NO_PROXY"]:
        os.environ["NO_PROXY"] = os.environ["NO_PROXY"] + ",api.siliconflow.cn"
else:
    os.environ["NO_PROXY"] = "api.siliconflow.cn"

_model = None
_model_lock = threading.Lock()

IS_SF = PROVIDER == "siliconflow"
MODEL_NAME = SF_MODEL if IS_SF else LOCAL_MODEL


def get_model():
    """懒加载并缓存本地模型（仅 local 模式）。"""
    global _model
    if _model is None:
        with _model_lock:
            if _model is None:
                from sentence_transformers import SentenceTransformer
                import torch
                torch.set_num_threads(max(1, (os.cpu_count() or 2) // 2))
                _model = SentenceTransformer(LOCAL_MODEL, device="cpu")
    return _model


def _embed_local(texts, normalize=False):
    model = get_model()
    vecs = model.encode(
        texts,
        normalize_embeddings=normalize,
        show_progress_bar=False,
        convert_to_numpy=True,
    )
    return [v.tolist() for v in vecs]


def _embed_siliconflow(texts, normalize=False):
    """调 SiliconFlow /v1/embeddings，分批 + 限流重试。返回 list[list[float]]。"""
    import requests

    if not SF_KEY:
        raise RuntimeError("SILICONFLOW_API_KEY 未设置，无法使用 siliconflow 后端")
    headers = {"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"}
    out = [None] * len(texts)
    batch_size = 32
    for i in range(0, len(texts), batch_size):
        batch = texts[i : i + batch_size]
        body = {
            "model": SF_MODEL,
            "input": batch,
            "encoding_format": "float",
            "dimensions": EMBED_DIM,
        }
        # 限流/超时重试（指数退避，最多 ~5 次）
        for attempt in range(6):
            try:
                r = requests.post(SF_API, headers=headers, json=body, timeout=60)
                if r.status_code == 200:
                    data = r.json()
                    for item in data.get("data", []):
                        idx = item.get("index", 0)
                        if i + idx < len(out):
                            out[i + idx] = item["embedding"]
                    break
                elif r.status_code in (429, 503, 504):
                    wait = min(2 ** attempt, 30)
                    time.sleep(wait)
                    continue
                else:
                    raise RuntimeError(f"SiliconFlow {r.status_code}: {r.text[:240]}")
            except requests.RequestException as e:
                wait = min(2 ** attempt, 30)
                time.sleep(wait)
                if attempt == 5:
                    raise RuntimeError(f"SiliconFlow 请求失败: {e}")
    # 兜底：任何未填充项用零向量占位（不应发生）
    for j, v in enumerate(out):
        if v is None:
            out[j] = [0.0] * EMBED_DIM
    if normalize:
        import math
        for v in out:
            norm = math.sqrt(sum(x * x for x in v))
            if norm > 0:
                v[:] = [x / norm for x in v]
    return out


def embed_texts(texts, normalize=False):
    """批量文本 → 向量列表（list[list[float]]）。"""
    if IS_SF:
        return _embed_siliconflow(texts, normalize)
    return _embed_local(texts, normalize)


def _model_dim():
    if IS_SF:
        return EMBED_DIM
    try:
        return int(get_model().get_sentence_embedding_dimension())
    except Exception:
        return 768


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "memoria-embed/1.1"

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
            self._send(200, {"status": "ok", "model": MODEL_NAME, "dim": _model_dim(),
                             "provider": PROVIDER})
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
    if IS_SF:
        print(f"[embed] provider=siliconflow model={SF_MODEL} dim={EMBED_DIM}", flush=True)
        print(f"[embed] key={'SET' if SF_KEY else 'MISSING!'} -> http://{HOST}:{PORT}/embed", flush=True)
    else:
        print(f"[embed] loading {LOCAL_MODEL} (offline mode)...", flush=True)
        dim = _model_dim()
        print(f"[embed] loaded {LOCAL_MODEL} ({dim}d) -> http://{HOST}:{PORT}/embed", flush=True)
    server = ThreadingHTTPServer((HOST, PORT), Handler)
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
