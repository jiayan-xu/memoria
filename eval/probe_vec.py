import os, sqlite3, struct

db = os.path.expanduser(r"~/.qclaw/workspace/memoria/data/memoria.db")
con = sqlite3.connect(db)
cur = con.cursor()
cur.execute("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('memory_vectors','memories')")
print("tables:", [r[0] for r in cur.fetchall()])
try:
    cur.execute("SELECT COUNT(*) FROM memory_vectors")
    print("memory_vectors count:", cur.fetchone()[0])
except Exception as e:
    print("memory_vectors err:", e)
try:
    cur.execute("SELECT COUNT(*) FROM memories")
    print("memories count:", cur.fetchone()[0])
except Exception as e:
    print("memories err:", e)
try:
    cur.execute("SELECT id, vector FROM memory_vectors LIMIT 8")
    rows = cur.fetchall()
    if not rows:
        print("memory_vectors empty")
    for rid, blob in rows:
        if blob is None:
            print("  id", rid, "blob=None")
        else:
            dim = len(blob)//4
            vals = struct.unpack('<%df'%min(dim,5), blob[:min(len(blob),20)])
            print("  id", rid, "bytes", len(blob), "dim", dim, "head", [round(v,3) for v in vals])
except Exception as e:
    print("sample err:", e)
# distinct dimension distribution
try:
    cur.execute("SELECT (LENGTH(vector)/4) AS d, COUNT(*) FROM memory_vectors GROUP BY d")
    print("dim distribution:", cur.fetchall())
except Exception as e:
    print("dim dist err:", e)
con.close()
