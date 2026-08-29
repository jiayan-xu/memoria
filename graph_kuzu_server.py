#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
memoria 时序知识图谱服务（Kuzu 嵌入式图库，端口 8779）
=====================================================
P2：把写入整合时抽取的实体/关系落到真图库，支持多跳遍历 + 时效过滤（as_of）。
由 memoria-server 的 consolidation 路径 fire-and-forget 写入；MCP 工具 memory_graph_query 代理查询。

运行：WorkBuddy Python 3.13（kuzu 无 cp314 wheel）：
  C:\\Users\\user\\.workbuddy\\binaries\\python\\versions\\3.13.12\\python.exe graph_kuzu_server.py

接口（仅回环，与 embed 服务同纪律）：
  GET  /health    -> {status, entities, relations}
  POST /upsert    {namespace, memory_id, entities:[{name,etype}], edges:[{subject,predicate,obj,valid_from,valid_to}]}
  POST /query     {cypher, as_of?}                    —— 任意只读 Cypher（需含 RETURN）
  POST /neighbors {name, depth?, as_of?, namespace?}  —— 多跳邻居（含时效过滤）
  POST /stats     {namespace?}                        —— 节点/边计数
"""
import os, sys, json, re, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import kuzu

DIR = os.path.dirname(os.path.abspath(__file__))
DB_PATH = os.environ.get("MEMORIA_GRAPH_DB", os.path.join(DIR, "data", "graph_kuzu"))
HOST = os.environ.get("MEMORIA_GRAPH_HOST", "127.0.0.1")   # 仅回环（同 embed 纪律）
PORT = int(os.environ.get("MEMORIA_GRAPH_PORT", "8779"))

if HOST != "127.0.0.1":
    sys.stderr.write(f"[graph] FATAL: 拒绝非回环绑定 {HOST!r}\n")
    sys.exit(2)

_db = kuzu.Database(DB_PATH)
_conn = kuzu.Connection(_db)
_wlock = threading.Lock()   # 写路径串行；查询走只读也共享此连接（低流量，够用）

def _init_schema():
    with _wlock:
        for ddl in (
            "CREATE NODE TABLE IF NOT EXISTS Entity(name STRING, etype STRING, namespace STRING, PRIMARY KEY(name))",
            "CREATE REL TABLE IF NOT EXISTS Relation(FROM Entity TO Entity, predicate STRING, memory_id STRING, namespace STRING, valid_from STRING DEFAULT '', valid_to STRING DEFAULT '')",
        ):
            _conn.execute(ddl)

_init_schema()

def rows_of(res):
    cols = res.get_column_names()
    out = []
    while res.has_next():
        row = res.get_next()
        out.append({cols[i]: row[i] for i in range(len(cols))})
    return out

def q(sql, params=None):
    res = _conn.execute(sql, parameters=params or {})
    return rows_of(res)

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
            try:
                ne = q("MATCH (e:Entity) RETURN COUNT(e) AS c")[0]["c"]
                nr = q("MATCH (:Entity)-[r:Relation]->(:Entity) RETURN COUNT(r) AS c")[0]["c"]
                self._send(200, {"status": "ok", "entities": ne, "relations": nr, "db": DB_PATH})
            except Exception as e:
                self._send(500, {"status": "error", "error": str(e)})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        path = self.path.split("?")[0]
        try:
            length = int(self.headers.get("Content-Length", "0") or "0")
            req = json.loads(self.rfile.read(length).decode("utf-8") or "{}") if length else {}
        except Exception as e:
            return self._send(400, {"ok": False, "error": f"bad request: {e}"})

        try:
            if path == "/upsert":
                return self._upsert(req)
            if path == "/query":
                return self._query(req)
            if path == "/neighbors":
                return self._neighbors(req)
            if path == "/stats":
                return self._stats(req)
            return self._send(404, {"error": "not found"})
        except Exception as e:
            return self._send(500, {"ok": False, "error": str(e)})

    def _upsert(self, req):
        ns = str(req.get("namespace") or "default")
        mid = str(req.get("memory_id") or "")
        ents = req.get("entities") or []
        edges = req.get("edges") or []
        with _wlock:
            for e in ents:
                name = str(e.get("name") or "").strip()
                if not name:
                    continue
                _conn.execute(
                    "MERGE (e:Entity {name: $name}) "
                    "ON CREATE SET e.etype = $etype, e.namespace = $ns "
                    "ON MATCH SET e.etype = $etype, e.namespace = $ns",
                    {"name": name[:120], "etype": str(e.get("etype") or "other")[:24], "ns": ns})
            # 幂等：同一 memory_id 的旧边先删再建
            if mid:
                _conn.execute("MATCH (:Entity)-[r:Relation {memory_id: $mid}]->(:Entity) DELETE r", {"mid": mid})
            for ed in edges:
                s = str(ed.get("subject") or "").strip()
                o = str(ed.get("obj") or ed.get("object") or "").strip()
                p = str(ed.get("predicate") or "related_to").strip()[:60]
                if not s or not o or s == o:
                    continue
                _conn.execute(
                    "MATCH (a:Entity {name: $s}), (b:Entity {name: $o}) "
                    "CREATE (a)-[:Relation {predicate: $p, memory_id: $mid, namespace: $ns, "
                    "valid_from: $vf, valid_to: $vt}]->(b)",
                    {"s": s[:120], "o": o[:120], "p": p, "mid": mid, "ns": ns,
                     "vf": str(ed.get("valid_from") or ""), "vt": str(ed.get("valid_to") or "")})
        return self._send(200, {"ok": True, "entities": len(ents), "edges": len(edges)})

    def _query(self, req):
        cy = str(req.get("cypher") or "").strip()
        if not cy:
            return self._send(400, {"ok": False, "error": "cypher required"})
        # 只读护栏 v2：单语句 + 全 token 扫描写关键词。首关键词检查可被
        # "MATCH (e) DETACH DELETE e" / "WITH 1 AS x CREATE ..." 绕过（ocr 审查发现）。
        body = cy.rstrip().rstrip(";")
        if ";" in body:
            return self._send(403, {"ok": False, "error": "read-only: single statement only"})
        head = body.split(None, 1)[0].upper()
        if head not in ("MATCH", "OPTIONAL", "WITH", "RETURN"):
            return self._send(403, {"ok": False, "error": "read-only: only MATCH/WITH/RETURN queries allowed"})
        write_kw = re.compile(r"\b(CREATE|MERGE|DELETE|DETACH|SET|DROP|REMOVE|CALL|LOAD|FOREACH)\b", re.IGNORECASE)
        if write_kw.search(body):
            return self._send(403, {"ok": False, "error": "read-only: write keywords are not allowed"})
        params = {}
        as_of = req.get("as_of")
        if as_of:
            params["as_of"] = str(as_of)
        with _wlock:
            res = _conn.execute(cy, parameters=params)
            rows = rows_of(res)
        return self._send(200, {"ok": True, "rows": rows})

    def _neighbors(self, req):
        name = str(req.get("name") or "").strip()
        if not name:
            return self._send(400, {"ok": False, "error": "name required"})
        depth = max(1, min(int(req.get("depth") or 2), 4))
        ns = str(req.get("namespace") or "default")
        as_of = req.get("as_of")
        # BFS 逐跳（kuzu 变长路径上的列表推导受限，Python 侧迭代简单可靠）
        seen = {name}
        frontier = [name]
        out = []
        with _wlock:
            for hop in range(1, depth + 1):
                if not frontier:
                    break
                nxt = []
                for cur in frontier:
                    cy = (
                        "MATCH (a:Entity {name: $cur})-[r:Relation]->(b:Entity) "
                        "WHERE r.namespace = $ns AND " +
                        # 时效语义：空串=开放端。有 as_of=区间包含；无 as_of=仅现行（valid_to 空
                        # 且 valid_from 已开始，防"未生效"边混入）
                        ("(r.valid_from = '' OR r.valid_from <= $as_of) AND (r.valid_to = '' OR r.valid_to >= $as_of) "
                         if as_of else
                         "(r.valid_to = '' AND (r.valid_from = '' OR r.valid_from <= date())) ") +
                        "RETURN b.name AS name, b.etype AS etype, r.predicate AS via"
                    )
                    params = {"cur": cur, "ns": ns}
                    if as_of:
                        params["as_of"] = str(as_of)
                    rows = rows_of(_conn.execute(cy, parameters=params))
                    for row in rows:
                        if row["name"] in seen:
                            continue
                        seen.add(row["name"])
                        nxt.append(row["name"])
                        out.append({"name": row["name"], "etype": row["etype"],
                                    "via": row["via"], "hops": hop})
                frontier = nxt
        return self._send(200, {"ok": True, "rows": out[:100]})

    def _stats(self, req):
        ns = req.get("namespace")
        params = {}
        w = ""
        if ns:
            w = "WHERE e.namespace = $ns"
            params["ns"] = str(ns)
        with _wlock:
            ne = q(f"MATCH (e:Entity) {w} RETURN COUNT(e) AS c", params)[0]["c"]
            wr = "WHERE r.namespace = $ns" if ns else ""
            nr = q(f"MATCH (:Entity)-[r:Relation]->(:Entity) {wr} RETURN COUNT(r) AS c", params)[0]["c"]
        return self._send(200, {"ok": True, "entities": ne, "relations": nr})

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    print(f"[graph] kuzu db={DB_PATH} -> http://{HOST}:{PORT} (loopback only)", flush=True)
    ThreadingHTTPServer((HOST, PORT), Handler).serve_forever()
