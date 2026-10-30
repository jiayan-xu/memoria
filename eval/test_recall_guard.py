#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""recall_guard 查询构造与分层抽样的纯标准库回归测试。"""
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import recall_guard as rg


class QueryConstructionTest(unittest.TestCase):
    def test_content_query_ignores_tags_and_normalizes_whitespace(self):
        content = "  第一行\n\t第二行  " + "长" * 80
        self.assertEqual(rg.extract_content_query(content),
                         "第一行 第二行 " + "长" * 32)

    def test_tag_query_filters_metadata_and_generic_tags(self):
        tags = '["signal:num:42","occurred:2026-10-30","auto_consolidated",'
        tags += '"pattern","项目A","召回"]'
        self.assertEqual(rg.extract_tag_query(tags), "项目A 召回")

    def test_tag_query_empty_after_filter(self):
        self.assertEqual(rg.extract_tag_query('["signal:date:2026-10-30","pattern"]'), "")
        self.assertEqual(rg.extract_tag_query("[]"), "")


class StratifiedSamplingTest(unittest.TestCase):
    def test_quota_largest_remainder(self):
        counts = {"empty": 6, "auto_consolidated": 3, "normal": 3}
        self.assertEqual(rg.stratified_quotas(counts, 6),
                         {"empty": 3, "auto_consolidated": 1, "normal": 2})
        self.assertEqual(sum(rg.stratified_quotas(counts, 80).values()), 12)
        self.assertEqual(rg.stratified_quotas(counts, 0),
                         {"empty": 0, "auto_consolidated": 0, "normal": 0})

    def test_sample_memories_uses_active_population_and_layers(self):
        with tempfile.TemporaryDirectory() as td:
            db = os.path.join(td, "guard.db")
            con = sqlite3.connect(db)
            con.execute("""CREATE TABLE memories (
                id TEXT PRIMARY KEY, namespace TEXT, content TEXT, tags TEXT,
                superseded_by TEXT, valid_to TEXT)""")
            rows = []
            for kind, tags, count in (
                    ("e", "[]", 6),
                    ("a", '["auto_consolidated"]', 3),
                    ("n", '["项目A"]', 3)):
                for i in range(count):
                    rows.append((f"{kind}{i}", rg.NS, f"content-{kind}-{i}", tags,
                                 None, None))
            rows.extend([
                ("superseded", rg.NS, "old", "[]", "e0", None),
                ("expired", rg.NS, "expired", "[]", None, "2000-01-01 00:00:00"),
            ])
            con.executemany("INSERT INTO memories VALUES (?,?,?,?,?,?)", rows)
            con.commit()
            con.close()

            sampled = rg.sample_memories(db, 6)
            self.assertEqual(len(sampled), 6)
            actual = {"empty": 0, "auto_consolidated": 0, "normal": 0}
            for _, _, tags in sampled:
                actual[rg.tag_kind(tags)] += 1
            self.assertEqual(actual, {"empty": 3, "auto_consolidated": 1, "normal": 2})
            self.assertNotIn("superseded", {r[0] for r in sampled})
            self.assertNotIn("expired", {r[0] for r in sampled})


if __name__ == "__main__":
    unittest.main()
