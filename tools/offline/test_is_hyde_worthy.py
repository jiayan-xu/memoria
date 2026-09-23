# -*- coding: utf-8 -*-
"""is_hyde_worthy 冒烟（不触发 main / 不读 API key）。"""
import importlib.util
import os
import sys

path = os.path.join(os.path.dirname(__file__), "build_hype_vectors.py")
# 避免 import 时误跑 main
spec = importlib.util.spec_from_file_location("hype_gate", path)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

cases = [
    ("用户偏好简洁裁决式结论，不要写成长篇报告。", True),
    ("列A\t列B\t列C\n" + "\t".join(["1", "2", "3"]) * 20, False),
    ("�" * 50 + "hello world", False),
    ("{'a':1,'b':2};" * 30, False),
    ("x" * 5, False),
    ("固废考核：卸料扬尘以垃圾吊坑内 F0/F3 为准，过车运动模糊不计入。", True),
]
failed = 0
for s, expect in cases:
    ok, why = m.is_hyde_worthy(s)
    status = "OK" if ok == expect else "FAIL"
    if ok != expect:
        failed += 1
    print(f"{status} worthy={ok} why={why!r} expect={expect}")
sys.exit(1 if failed else 0)
