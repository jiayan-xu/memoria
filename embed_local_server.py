#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
memoria 本地嵌入服务 v1.0（Qwen3-Embedding-0.6B int8 ONNX，端口 8778）
=====================================================================
替代 :8777 的 siliconflow 出网方案：零出网、零 API 成本、P50 ~150ms（原 534ms+）。
原生 1024d = memoria hnsw.rs DIM，无需改 Rust。
last-token pooling（Qwen3-Embedding 官方约定），L2 归一化。
契约与 embed_server.py 相同：POST /embed {texts:[...]} → {embeddings, dim, model}；GET /health。
⚠ 换模型 = 语义空间变更：必须全量重嵌 memory_vectors + 重建 HNSW（memoria 启动自动 rebuild）。
"""
import os, sys, json, time, threading
import numpy as np
import onnxruntime as ort
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "embed-local")
MODEL_PATH = os.path.join(DIR, "onnx_model_int8.onnx")
TOKENIZER = os.path.join(DIR, "tokenizer.json")
HOST = os.environ.get("MEMORIA_EMBED_HOST", "127.0.0.1")   # 与 8777 同规则：仅回环
PORT = int(os.environ.get("MEMORIA_EMBED_PORT", "8778"))
MAX_LEN = 2048

os.environ.setdefault("ORT_GLOBAL_THREAD_POOL", "4")
_so = ort.SessionOptions()
_so.intra_op_num_threads = 2   # 单请求 2 线程，余量留给并发请求并行
_sess = ort.InferenceSession(MODEL_PATH, sess_options=_so, providers=["CPUExecutionProvider"])
from tokenizers import Tokenizer
_tok = Tokenizer.from_file(TOKENIZER)
print(f"[embed-local] Qwen3-Embedding-0.6B int8 loaded, dim=1024 -> http://{HOST}:{PORT}/embed", flush=True)

_lock = threading.Lock()  # ORT session 并发 run 线程安全，锁只为控制 CPU 抢占抖动

def embed_batch(texts):
    out = []
    # 无全局锁：ORT session 并发 run 线程安全，放开多请求真正并行吃多核
    for t in texts:
            enc = _tok.encode(t)
            ids = enc.ids[:MAX_LEN]
            n = len(ids)
            feed = {
                "input_ids": np.array([ids], dtype=np.int64),
                "attention_mask": np.array([[1] * n], dtype=np.int64),
                "position_ids": np.array([list(range(n))], dtype=np.int64),
            }
            for i in range(28):
                feed[f"past_key_values.{i}.key"] = np.zeros((1, 8, 0, 128), dtype=np.float32)
                feed[f"past_key_values.{i}.value"] = np.zeros((1, 8, 0, 128), dtype=np.float32)
            h = _sess.run(["last_hidden_state"], feed)[0]
            v = h[0, n - 1, :].astype(np.float32)
            v /= (np.linalg.norm(v) + 1e-12)
            out.append(v.tolist())
    return out

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

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
            self._send(200, {"status": "ok", "model": "Qwen3-Embedding-0.6B-int8-local", "dim": 1024, "provider": "local-qwen3"})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path.split("?")[0] != "/embed":
            self._send(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
            req = json.loads(self.rfile.read(length).decode("utf-8") or "{}") if length else {}
        except Exception as e:
            self._send(400, {"error": f"bad request: {e}"})
            return
        texts = req.get("texts")
        if not isinstance(texts, list) or not all(isinstance(t, str) for t in texts):
            self._send(400, {"error": "`texts` must be a list of strings"})
            return
        if not texts:
            self._send(200, {"embeddings": [], "dim": 0, "model": "local-qwen3"})
            return
        try:
            embs = embed_batch(texts)
        except Exception as e:
            self._send(500, {"error": f"encode failed: {e}"})
            return
        self._send(200, {"embeddings": embs, "dim": len(embs[0]) if embs else 0,
                         "model": "Qwen3-Embedding-0.6B-int8-local"})

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    if HOST != "127.0.0.1":
        sys.stderr.write(f"[embed-local] FATAL: 拒绝非回环绑定 {HOST!r}\n")
        sys.exit(2)
    ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
