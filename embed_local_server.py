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
# 2026-08-29 模型切换：Qwen3-0.6B-int8 召回不达标（内存 A/B MRR 0.688 vs bge-m3 0.874，
# recall@5 0.846 vs 0.962——见 embed_ab_test.py），默认换 bge-m3-int8（CLS pooling，1024d）。
# MEMORIA_EMBED_LOCAL_MODEL=qwen3 可切回。
WHICH = os.environ.get("MEMORIA_EMBED_LOCAL_MODEL", "bge-m3").lower()

MODELS = {
    "bge-m3": {
        "onnx": "bge-m3_model_int8.onnx", "tokenizer": "bge-m3_tokenizer.json",
        "pooling": "cls", "layers": 0, "kv_heads": 0, "head_dim": 0, "name": "bge-m3-int8-local",
    },
    "qwen3": {
        "onnx": "onnx_model_int8.onnx", "tokenizer": "tokenizer.json",
        "pooling": "last", "layers": 28, "kv_heads": 8, "head_dim": 128, "name": "Qwen3-Embedding-0.6B-int8-local",
    },
}
_cfg = MODELS.get(WHICH) or MODELS["bge-m3"]

MODEL_PATH = os.path.join(DIR, _cfg["onnx"])
TOKENIZER = os.path.join(DIR, _cfg["tokenizer"])
# ── /rerank（2026-10-02 新增）──────────────────────────────────────────────
# 背景：本地无 cross-encoder 模型（原 /rerank 走 SiliconFlow 出网，已随零出网方案废弃），
# 而 memoria 侧注释明确「rerank 是召回质量主引擎，必须保留」。故在此以已加载的 bge-m3
# 双编码器实现：score = W_KW * 字面重叠 + (1-W_KW) * 归一化余弦。
#   · 保留关键词权重（默认 0.6，与 MEMORIA_RERANK_W_KW 对齐），避免纯语义重排压掉关键词召回
#   · 截断长文本控延迟，确保落在 memoria 客户端 5s 超时内
# ⚠️ 契约（与 src/mcp_server.rs::rerank_query 对齐）：
#    请求 {"query": str, "documents": [str]} → 响应 {"results":[{"index","relevance_score"}]}
#    Rust 对「未出现在 results 里的文档」记 0.0 分后再降序 ⇒ **必须给全部文档打分**，
#    否则未返回的候选会被打到末尾（比不重排更糟）。
RERANK_W_KW = float(os.environ.get("MEMORIA_RERANK_W_KW", "0.6"))
RERANK_MAX_CHARS = int(os.environ.get("MEMORIA_RERANK_MAX_CHARS", "256"))
HOST = os.environ.get("MEMORIA_EMBED_HOST", "127.0.0.1")   # 与 8777 同规则：仅回环
PORT = int(os.environ.get("MEMORIA_EMBED_PORT", "8778"))
MAX_LEN = 512 if _cfg["pooling"] == "cls" else 2048

os.environ.setdefault("ORT_GLOBAL_THREAD_POOL", "4")
_so = ort.SessionOptions()
_so.intra_op_num_threads = 2   # 单请求 2 线程，余量留给并发请求并行
_sess = ort.InferenceSession(MODEL_PATH, sess_options=_so, providers=["CPUExecutionProvider"])
from tokenizers import Tokenizer
_tok = Tokenizer.from_file(TOKENIZER)
print(f"[embed-local] {_cfg['name']} ({_cfg['pooling']} pooling) loaded, dim=1024 -> http://{HOST}:{PORT}/embed", flush=True)

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
        }
        if _cfg["pooling"] == "cls":
            # bge-m3 / XLM-R encoder：CLS token + L2 归一
            h = _sess.run(["last_hidden_state"], feed)[0]
            v = h[0, 0, :].astype(np.float32)
        else:
            # Qwen3 decoder：last-token pooling，需空 past KV + position_ids 做一次 prefill
            feed["position_ids"] = np.array([list(range(n))], dtype=np.int64)
            for i in range(_cfg["layers"]):
                feed[f"past_key_values.{i}.key"] = np.zeros((1, _cfg["kv_heads"], 0, _cfg["head_dim"]), dtype=np.float32)
                feed[f"past_key_values.{i}.value"] = np.zeros((1, _cfg["kv_heads"], 0, _cfg["head_dim"]), dtype=np.float32)
            h = _sess.run(["last_hidden_state"], feed)[0]
            v = h[0, n - 1, :].astype(np.float32)
        v /= (np.linalg.norm(v) + 1e-12)
        out.append(v.tolist())
    return out

def embed_batch_fast(texts, max_len=None):
    """真批处理编码：单次 ORT 前向，消除 per-call 固定开销（约 7x 加速），供 /rerank 使用。

    右 padding + attention_mask；CLS pooling 取第 0 位，padding 不影响语义。
    注意：计算量 ∝ batch × seq_len，故调用方需配合 RERANK_MAX_CHARS 截断控延迟。
    """
    max_len = max_len or MAX_LEN
    encs = [_tok.encode(t).ids[:max_len] for t in texts]
    L = max([len(e) for e in encs] + [1])
    ids = np.zeros((len(encs), L), dtype=np.int64)
    am = np.zeros((len(encs), L), dtype=np.int64)
    for i, e in enumerate(encs):
        if e:
            ids[i, :len(e)] = e
            am[i, :len(e)] = 1
    if _cfg["pooling"] != "cls":
        raise RuntimeError("embed_batch_fast 仅支持 cls pooling")
    h = _sess.run(["last_hidden_state"], {"input_ids": ids, "attention_mask": am})[0]
    v = h[:, 0, :].astype(np.float32)
    v /= (np.linalg.norm(v, axis=1, keepdims=True) + 1e-12)
    return v


def _bigrams(s: str) -> set:
    """字符二元组集合（去空白），中文无空格分词场景下的轻量字面特征。"""
    s = "".join(ch for ch in s if not ch.isspace())
    if len(s) < 2:
        return {s} if s else set()
    return {s[i:i + 2] for i in range(len(s) - 1)}


def _lexical_score(q: str, d: str) -> float:
    """Dice 系数，0~1；任一为空则为 0。"""
    a, b = _bigrams(q), _bigrams(d)
    if not a or not b:
        return 0.0
    return 2.0 * len(a & b) / (len(a) + len(b))


def rerank_scores(query: str, docs: list) -> list:
    """双编码器重排：返回与 docs 等长的分数列表（已融合字面重叠）。"""
    q = (query or "")[:RERANK_MAX_CHARS]
    ds = [(d or "")[:RERANK_MAX_CHARS] for d in docs]
    v = embed_batch_fast([q] + ds)
    cos = v[1:] @ v[0]                           # 已 L2 归一 ⇒ 点积即余弦
    cos01 = (cos + 1.0) / 2.0                    # 归一到 0~1 便于与字面分同权融合
    return [
        RERANK_W_KW * _lexical_score(q, d) + (1.0 - RERANK_W_KW) * float(cos01[i])
        for i, d in enumerate(ds)
    ]


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
            self._send(200, {"status": "ok", "model": _cfg["name"], "dim": 1024, "provider": "local-" + WHICH})
        else:
            self._send(404, {"error": "not found"})

    def _read_json(self):
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
            return json.loads(self.rfile.read(length).decode("utf-8") or "{}") if length else {}
        except Exception as e:
            self._send(400, {"error": f"bad request: {e}"})
            return None

    def _handle_rerank(self, req):
        query = req.get("query")
        docs = req.get("documents")
        if not isinstance(docs, list):      # 容忍旧字段 docs
            docs = req.get("docs")
        if not isinstance(query, str) or not isinstance(docs, list) \
                or not all(isinstance(d, str) for d in docs):
            self._send(400, {"error": "`query` must be str and `documents` a list of strings"})
            return
        if not docs:
            self._send(200, {"results": []})
            return
        try:
            scores = rerank_scores(query, docs)
        except Exception as e:
            self._send(500, {"error": f"rerank failed: {e}"})
            return
        order = sorted(range(len(docs)), key=lambda i: scores[i], reverse=True)
        self._send(200, {
            "results": [{"index": i, "relevance_score": scores[i]} for i in order],
            "model": _cfg["name"] + "-biencoder",
            "rerank_w_kw": RERANK_W_KW,
        })

    def do_POST(self):
        path = self.path.split("?")[0]
        if path == "/rerank":
            req = self._read_json()
            if req is not None:
                self._handle_rerank(req)
            return
        if path != "/embed":
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
                         "model": _cfg["name"]})

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    if HOST != "127.0.0.1":
        sys.stderr.write(f"[embed-local] FATAL: 拒绝非回环绑定 {HOST!r}\n")
        sys.exit(2)
    ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
