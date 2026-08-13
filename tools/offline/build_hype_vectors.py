#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
V1（2026-08-12）：HyPE 假设问句补嵌脚本（A/B 验证用）。

对 golden set（eval/hyde_queries.json）的记忆生成「假设问句」并嵌入，写入
memory_hype_vectors 表（与 memory_vectors 平行）。semantic_search 双路合并后
可验证 HyPE 是否提升 recall（论文：IEEE Access 2025, +42pp precision）。

关键纪律：
- **绝不用 golden 的 query 作为假设问句**（那是评测集，直接用了 = 数据泄漏/作弊）。
  假设问句由 LLM 独立生成（模拟「用户会怎么问」），与 golden query 措辞不同。
- 生成失败/嵌入失败 → 跳过该条（记日志），不中断、不伪造。
- 幂等：已存在 id 的 hype 向量会 UPDATE（重跑安全）。
- 只写新表，不动 memory_vectors / memories / HNSW 内容索引。

用法：
  python tools/offline/build_hype_vectors.py            # 默认 golden set 58 条
  python tools/offline/build_hype_vectors.py --limit 10 # 只处理前 10 条（调试）
  python tools/offline/build_hype_vectors.py --all      # 全库 5339 条（验证通过后）
环境：SILICONFLOW_API_KEY（.env 或环境变量）；复用 embed_server 的 chat/embed 通道。
"""
import os
import sys
import json
import time
import struct
import sqlite3
import urllib.request
import urllib.error
import argparse

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, "..", ".."))
# 默认 DB 相对仓库内 data/（与代码库其余脚本一致），而非依赖外层目录恰好叫
# "memoria"——否则换目录名会静默写到错误的兄弟路径（#R35 maintainability/low）。
DB = os.environ.get(
    "MEMORIA_DB_PATH",
    os.path.join(REPO, "data", "memoria.db"),
)
GOLDEN = os.path.join(REPO, "eval", "hyde_queries.json")
# .env 优先环境变量，其次仓库同级 memoria/ 的运行时镜像（与 recall_guard 的 resolve 一致）
ENV = os.environ.get(
    "MEMORIA_ENV_PATH",
    os.path.join(os.path.dirname(REPO), "memoria", ".env"),
)

EMBED_URL = os.environ.get("MEMORIA_EMBEDDING_URL", "http://127.0.0.1:8777/embed")
# Rust 侧 HNSW 维度硬约束（src/vector/hnsw.rs DIM=1024）。仅校验 embed_server 返回的 dim
# 不够——服务器可能跑 local 模型（text2vec 768d），len(v)==dim 通过但 rebuild 侧
# v.len()!=DIM 会静默跳过每一行，整批"看起来成功"而索引恒空（#R34 bug/medium）。
# #R53 bug/medium：**硬编码 1024，不读 MEMORIA_EMBED_DIM**——该 env 同时配置
# embed_server 的输出维度（embed_server.py:83 默认 768，注释还声称 768 对齐 hnsw.rs）；
# 共用同一 knob 会让运维按 embed_server 注释设 MEMORIA_EMBED_DIM=768 时 RUST_DIM 也变
# 768：预检与逐行 len 检查全部通过、768d 向量落库，而 Rust rebuild（DIM=1024）静默
# 跳过每一行——"写入 N / 失败 0"假成功恰好被同一配置制造出来。守卫常量必须独立于
# 模型输出维度 knob。
RUST_DIM = 1024
CHAT_URL = os.environ.get(
    "SILICONFLOW_CHAT_URL", "https://api.siliconflow.cn/v1/chat/completions"
)
CHAT_MODEL = os.environ.get("MEMORIA_HYDE_MODEL", "Qwen/Qwen2.5-7B-Instruct")


def get_secret(name, path):
    try:
        for line in open(path, encoding="utf-8-sig"):
            line = line.strip()
            if line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            if k.strip() == name:
                return v.strip().strip('"').strip("'")
    except FileNotFoundError:
        pass
    return None


SF_KEY = os.environ.get("SILICONFLOW_API_KEY", "") or get_secret("SILICONFLOW_API_KEY", ENV) or ""


class DeterministicSkipError(RuntimeError):
    """确定性失败（配置错误/内容过滤拒绝等）：重跑永远同样失败，按 skip 计数。

    #R45 bug/medium：主循环的 catch-all `except Exception` 曾把 generate_question/embed
    抛出的确定性错误（非 429 4xx：401 无效 key、400 未知模型、内容过滤拒绝）重新归类
    为"重试耗尽瞬时故障"写入 fail_ids——这类 id 每次重跑同样失败，清单永不收敛且每轮
    白付付费调用，直接违反"fail_ids 只含可修复瞬时失败"的契约。专用异常类型让主循环
    用独立 except 分支区分：确定性 → skip（计数 skip_ids），瞬时 → fail（record_fail）。
    """


def generate_question(content: str) -> str:
    """LLM 生成「用户会怎么问才能找到这条记忆」的假设问句（与 golden query 独立）。"""
    sys_prompt = (
        "你是一个记忆检索优化器。给定一条知识库中的事实/记忆文本，请写一句"
        "「用户将来会怎么提问来检索这条信息」的自然问句。要求：\n"
        "1) 用口语化、自然的方式提问，不要照搬原文措辞；\n"
        "2) 问句要具体，能通过这条记忆回答；\n"
        "3) 只输出这一句问句，不要解释、不要引号、不要编号。\n"
        "若原文明显不可检索（纯代码/乱码），输出空字符串。"
    )
    body = {
        "model": CHAT_MODEL,
        "messages": [
            {"role": "system", "content": sys_prompt},
            {"role": "user", "content": content},
        ],
        "max_tokens": 120,
        "temperature": 0.7,
        "stream": False,
    }
    headers = {"Authorization": f"Bearer {SF_KEY}", "Content-Type": "application/json"}
    for attempt in range(3):
        try:
            req = urllib.request.Request(
                CHAT_URL, data=json.dumps(body).encode(), headers=headers, method="POST"
            )
            with urllib.request.urlopen(req, timeout=30) as r:
                try:
                    resp = json.loads(r.read().decode())
                except json.JSONDecodeError as e:
                    # #R55 bug/medium：2xx 非 JSON 体是确定性畸形响应（同形状每次必现）——
                    # 直接确定性跳过，不重试（此前 JSONDecodeError 落 catch-all 重试 3 次
                    # 白付 3 次付费调用后进 fail_ids，清单永不收敛；与 embed() 的
                    # #R53 同款分类）。
                    raise DeterministicSkipError(f"chat 响应非 JSON: {e}")
                if not isinstance(resp, dict):
                    # 2xx 非 dict 体（如 list）→ resp.get 会 AttributeError——确定性畸形。
                    raise DeterministicSkipError(f"chat 响应非 dict: {type(resp).__name__}")
            # #R40 bug/medium：OpenAI 兼容端点的 content 可为 null/missing（refusal/空回复）——
            # 无条件 .strip() 会 AttributeError/KeyError，3 次重试后被误判为"瞬时失败"
            # 进 fail_ids（永不收敛且每轮白付 3 次付费调用）。null/缺失是**确定性**结果，
            # 直接返回 ""（走确定性 skip 分支）。
            # #R42 bug/low：choices 空数组（content-filter refusal）或非 dict 元素也是
            # 确定性响应——重试 3 次纯白付。校验响应形态，空/畸形按确定性 skip 返回 ""。
            choices = resp.get("choices")
            if not isinstance(choices, list) or not choices:
                return ""
            msg = choices[0]
            if not isinstance(msg, dict):
                return ""
            content_val = msg.get("message")
            if not isinstance(content_val, dict):
                return ""
            content_val = content_val.get("content")
            # #R48 bug/low：content 可能是**非字符串结构**（content-parts 数组/
            # 结构化 refusal）——无条件 str() 会把 list 转成 `[{'type': 'text', ...}]`
            # 这类 repr，长度 ≥6 通过校验后被当作伪问句嵌入持久化，污染 HyPE 召回。
            # 非 str 按确定性 skip 返回 ""（与 null/缺失/空 choices 同语义）。
            if not isinstance(content_val, str):
                return ""
            return content_val.strip().strip('"').strip("'")
        except urllib.error.HTTPError as e:
            # #R41 bug/medium：4xx（除 429 限流）是确定性配置错误（401 无效 key、400
            # 未知模型/超长内容）——重试/重跑永远同样失败，直接 raise 免白付 3 次付费
            # 调用且不污染 fail_ids（该文件承诺只含可修复的瞬时失败）。
            # #R45 bug/medium：抛 DeterministicSkipError——主循环据此按 skip 计数而非
            # 记入 fail_ids（catch-all except Exception 会把 RuntimeError 重新归类为
            # 瞬时失败，让 400 之类的每轮重跑同样失败、清单永不收敛）。
            # #R47 bug/medium：确定性分支**只限 4xx**（除 429）——5xx（500/502/503/504）
            # 是瞬时过载/服务故障（sibling eval_hyde_recall.py 明确重试 429/503/504，
            # SiliconFlow chat 高负载常见 5xx）：按确定性跳过会静默丢记忆且无法经
            # fail_ids 定点重跑，违反"fail_ids 只含可修复瞬时失败"契约。5xx 走下方
            # 重试/瞬时失败路径。
            if 400 <= e.code < 500 and e.code != 429:
                raise DeterministicSkipError(f"chat HTTP {e.code}: {e}") from e
            if attempt == 2:
                raise RuntimeError(f"chat HTTP {e.code} 重试耗尽: {e}") from e
            time.sleep(min(2 ** attempt, 10))
        except DeterministicSkipError:
            # #R56 bug/medium：确定性错误（2xx 非 JSON/非 dict，同形状每次必现）**直接
            # 穿透不重试**——它是 RuntimeError 子类，落下面 except Exception 会重试 3
            # 次（每次白付一次付费 chat 调用）且末次 re-raise 后被主循环归为瞬时失败
            # 进 fail_ids（清单永不收敛）；与 embed() 的 #R53 前置子句同款。
            # 必须排在 except Exception **之前**（Python 按序匹配，先命中先处理）。
            raise
        except Exception as e:
            if attempt == 2:
                # #R38 bug/medium：重试耗尽 = 瞬时故障（网络/API 抖动），可重跑修复——
                # 必须 raise 让调用方记入 fail_ids，不能与"LLM 合法返回空"（确定性不可
                # 检索，重跑永远同样失败）混为同一 skip 分支。
                raise RuntimeError(f"chat 重试耗尽: {e}") from e
            time.sleep(min(2 ** attempt, 10))


def embed(texts):
    """调本地 embed_server（siliconflow Qwen3-VL-8B），返回 (vectors, dim)。

    与 generate_question 同款重试纪律：全库 --all 一次跑 5339 行，单 POST 无重试时
    任何瞬时网络/服务抖动都会永久漏掉那些记忆（--all 缺口无法定点修复）。
    3 次尝试 + 指数退避；最终失败抛异常由调用方计数（并记录失败 id，见 fail_ids）。
    """
    body = {"texts": texts, "normalize": False}
    headers = {"Content-Type": "application/json"}
    for attempt in range(3):
        try:
            req = urllib.request.Request(
                EMBED_URL, data=json.dumps(body).encode(), headers=headers, method="POST"
            )
            with urllib.request.urlopen(req, timeout=60) as r:
                try:
                    resp = json.loads(r.read().decode())
                except json.JSONDecodeError as e:
                    # #R53 bug/low：非 JSON 的 2xx 体是确定性畸形响应（同形状每次必现）——
                    # 直接抛确定性异常，不重试（此前 JSONDecodeError 经 3 次重试后
                    # 以 RuntimeError 被归为瞬时失败进 fail_ids，清单永不收敛）。
                    raise DeterministicSkipError(f"embed 响应非 JSON: {e}")
                if not isinstance(resp, dict):
                    # 2xx 但体不是 dict（如 list）——确定性畸形，同样不重试。
                    raise DeterministicSkipError(f"embed 响应非 dict: {type(resp).__name__}")
            # #R50 bug/medium：2xx 但缺 embeddings key 是**确定性**畸形响应（服务配置
            # 错误，同形状响应每次必现）——抛 DeterministicSkipError 计 skip；此前
            # KeyError 经 3 次重试后变 RuntimeError 被主循环归为瞬时失败记入 fail_ids
            # （永不收敛 + 每轮白付 chat/embed 调用）。
            try:
                vecs = resp["embeddings"]
            except KeyError:
                raise DeterministicSkipError("embed 响应缺 embeddings 字段（服务配置错误）")
            return vecs, resp.get("dim", 1024)
        except urllib.error.HTTPError as e:
            # 4xx（除 429）为确定性配置错误：直接 raise（与 generate_question 同款纪律，
            # #R45：抛 DeterministicSkipError 供主循环按 skip 计数）。
            # #R47 bug/medium：确定性分支**只限 4xx**——embed_server 把上游/编码失败
            # 映射为 500（embed_server.py:338），属瞬时；按确定性跳过会静默丢记忆且
            # 无定点重跑记录（skip_ids 不进 fail_ids）。
            if 400 <= e.code < 500 and e.code != 429:
                raise DeterministicSkipError(f"embed HTTP {e.code}: {e}") from e
            if attempt == 2:
                # #R41 maintainability/low：最终 attempt 内 raise 保留原始 traceback
                # （urlopen/JSON 解析/取字段的失败点）——循环后 raise last_err 会把
                # 栈指向 raise 行而非真实失败处，--all 5000+ 行时诊断困难。
                raise RuntimeError(f"embed HTTP {e.code} 重试耗尽: {e}") from e
            time.sleep(min(2 ** attempt, 10))
        except DeterministicSkipError:
            # #R50 bug/medium：确定性错误（4xx 配置错/畸形响应缺字段）**穿透重试**——
            # 重试同样失败，直接 raise 让主循环按 skip 计数。
            raise
        except Exception as e:
            if attempt == 2:
                raise RuntimeError(f"embed 重试耗尽: {e}") from e
            time.sleep(min(2 ** attempt, 10))
    # 不可达（每轮必 return 或 raise），保留以防未来改动破坏不变式。
    raise RuntimeError("embed: unreachable")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=None, help="只处理前 N 条")
    ap.add_argument(
        "--ids",
        default=None,
        help=f"定点重跑：读取 id 清单文件（默认清单位置: {os.path.join(HERE, 'hype_failed_ids.txt')}，"
        "或任意逗号分隔 id 列表文件）仅处理这些 id。**必须与 --all 组合**（fail_ids "
        "清单只由全量运行产生；独立 --ids 会对 golden 集过滤、报'已删除/失效'后空目标退出）",
    )
    ap.add_argument("--all", action="store_true", help="全库补嵌（验证通过后）")
    ap.add_argument("--db", default=DB)
    ap.add_argument("--dry-run", action="store_true", help="只生成问句不写库")
    args = ap.parse_args()

    if not SF_KEY:
        print("错误: SILICONFLOW_API_KEY 未配置（.env 或环境变量）")
        sys.exit(1)

    # ── 目标记忆列表 ──
    # #R37 bug/low：--all 的 DB 访问也须在 dry-run 守卫后——否则 --all --dry-run 仍会
    # connect（创建不存在的 DB 文件）并 SELECT（无 memories 表时裸抛
    # OperationalError，dry-run 带副作用崩溃）。dry-run 一律用 golden 目标
    # （只需展示问句生成，不需要全库枚举）。
    # #R42 bug/low：--all 非 dry-run 的 connect 前先做**存在性检查**——DB 缺失/路径
    # 错误时 connect 会静默创建空文件，随后 SELECT memories 裸抛 traceback 且留下
    # 副作用空库。明确诊断先行。
    if args.all and not args.dry_run:
        if not os.path.exists(args.db):
            print(f"错误: 数据库不存在 {args.db}（请检查 MEMORIA_DB_PATH / --db）")
            sys.exit(1)
        con = sqlite3.connect(args.db)
        try:
            # #R43 bug/low：补充 valid_to 过期过滤——过期记忆（valid_to < now）经
            # hybrid.rs 的 is_latest_now 在输出时被丢弃，为其生成问句+嵌入纯属
            # 付费浪费（sibling rebuild_vectors.py 与 eval 脚本均应用同款过滤）。
            # #R44 bug/low：`datetime('now')` 返回空格分隔 "YYYY-MM-DD HH:MM:SS"，
            # 而 memories.valid_to 存的是 'T' 分隔 ISO-8601（代码库约定补 T）——
            # 字符串比较里 'T'(0x54) > ' '(0x20)，同日到期值会错序：今日早些时候
            # 过期的记忆（"2026-08-12T08:00:00" vs now "2026-08-12 23:45:00"）仍
            # 通过 `valid_to > now` 被嵌入，违背"避免过期记忆付费"意图。用
            # strftime 生成 T 格式 now 与存储格式同构比较（eval_hyde_recall.py 同款）。
            rows = con.execute(
                "SELECT id, content FROM memories WHERE namespace='agent/xujiayan' "
                "AND superseded_by IS NULL "
                "AND (valid_to IS NULL OR valid_to='' OR valid_to > strftime('%Y-%m-%dT%H:%M:%S','now')) "
                "AND length(content) BETWEEN 10 AND 500"
            ).fetchall()
        except sqlite3.Error as e:
            con.close()
            print(f"错误: 读取 {args.db} 的 memories 表失败（库未初始化/路径错误/损坏）: {e}")
            sys.exit(1)
        con.close()
        targets = [(mid, c) for mid, c in rows]
        print(f"全库目标: {len(targets)} 条")
    else:
        # #R43 maintainability/low：golden 文件（gitignore，仅 eval/eval_hyde_recall.py
        # --build 生成）缺失时给友好诊断而非裸 FileNotFoundError traceback；with 块
        # 确保句柄关闭（镜像 --all 路径的错误处理）。
        if not os.path.exists(GOLDEN):
            print(f"错误: golden 文件不存在 {GOLDEN}（请先用 eval/eval_hyde_recall.py --build 生成）")
            sys.exit(1)
        try:
            with open(GOLDEN, encoding="utf-8") as f:
                d = json.load(f)
            targets = [(it["id"], it["content"]) for it in d["items"]]
        except (json.JSONDecodeError, KeyError, TypeError) as e:
            print(f"错误: golden 文件格式损坏/结构不符 {GOLDEN}: {e}")
            sys.exit(1)
        if args.all and args.dry_run:
            print(f"--all --dry-run: 使用 golden 目标 {len(targets)} 条（不读库）")
        else:
            print(f"golden 目标: {len(targets)} 条")

    # #R53 maintainability/low：--ids 定点重跑——从文件读 id 清单（每行一个 id），
    # 从全库/golden 目标中筛出这些 id（不存在的 id 忽略并提示）。闭环
    # hype_failed_ids.txt 的"可定点重跑"承诺：失败清单可低成本重试，无需全量 --all
    # 重付几千次付费调用。
    if args.ids:
        ids_file = args.ids
        if not os.path.exists(ids_file):
            print(f"错误: id 清单文件不存在 {ids_file}")
            sys.exit(1)
        with open(ids_file, encoding="utf-8") as f:
            # #R54 maintainability/low：支持逗号分隔（帮助文本承诺）——每行按逗号
            # split 后再收进集合（此前整行作一个 id，逗号文件全被当作不存在的 id）。
            wanted = set()
            for ln in f:
                for part in ln.strip().split(","):
                    part = part.strip()
                    if part:
                        wanted.add(part)
        by_id = {mid: c for mid, c in targets}
        targets = [(m, by_id[m]) for m in wanted if m in by_id]
        missing = wanted - set(by_id)
        if missing:
            print(f"提示: {len(missing)} 个 id 不在目标集中（已删除/失效），忽略")
        print(f"--ids 定点目标: {len(targets)} 条")
    if args.limit is not None:
        targets = targets[: args.limit]

    # #R58 bug/low：**空目标 fail-fast 前置**——--ids 全部失效/--all 0 行（库错/
    # ns 过滤不匹配）时在此退出：付费 preflight（embed + chat 各一次）尚未发生，
    # fail-fast-before-spending 覆盖空目标（原检查在 preflight 之后，白付两次调用）。
    if not targets:
        print(f"错误: 目标集为空（{args.db} 无匹配行 / --ids 全部失效 / ns 过滤不匹配）——已中止")
        sys.exit(1)

    ok = skip = fail = 0
    fail_ids = []
    skip_ids = []
    # #R62 bug/high：con 在此初始化（preflight 后段另有赋值）——atexit 注册段对
    # 非 --all 模式（golden/--ids）引用 con 时未定义 → UnboundLocalError 崩溃
    # 脚本主用法；--all 模式则避免误用旧连接对象。
    # #R64 bug/high：guard **不能检查 con**（刚置 None 恒 False → atexit 永不
    # 注册、中断保护死代码）；handler 只引用 args/pending_batch/failed_ids_path，
    # 条件只需 `not args.dry_run`（dry-run 不写库无 pending）。
    con = None
    if not args.dry_run:
        # #R61 bug/low：**中断安全**——Ctrl+C/SIGINT 时最后未提交批（最多 49 条
        # 已付费行）被解释器回滚且不进 fail_ids（record_fail 的"中断不丢清单"
        # 承诺只覆盖已 append 的 id）；atexit 在退出路径（含 KeyboardInterrupt
        # 传播）把 pending 冲进清单——付费工作不静默丢失。
        import atexit

        def _flush_pending_on_exit():
            # #R62 bug/low：**全量门控**——fail_ids 清单只属于全量运行（#R46/#R48）；
            # 调试运行（--limit N）中断时把子集 id 追加进去会污染累积清单。
            if not (args.all and args.limit is None):
                return
            try:
                if pending_batch:
                    with open(failed_ids_path, "a", encoding="utf-8") as f:
                        for pid in pending_batch:
                            f.write(pid + "\n")
                    print(f"  [warn] 中断退出: {len(pending_batch)} 条未提交行已追加到 fail_ids")
            except OSError as e:
                print(f"  [warn] 中断退出时写 fail_ids 失败: {e}")

        atexit.register(_flush_pending_on_exit)
    # #R60：**待提交批跟踪**——本批已 execute 未 commit 的 id；批/最终 commit 失败
    # 时这些行未落库（回滚丢弃），必须补记 fail_ids 否则静默丢失且无重跑路径。
    pending_batch = []
    # 已知限制（#R40 performance/low）：每轮一次 chat + 一次 embed 串行（--all 约 1 万次
    # 顺序请求）；embed 已支持 list 可批 16（仿 rebuild_vectors.py），但批量化需重构
    # 失败归因（批内单条失败 → 单独重嵌该条），留待后续优化，本轮保持正确性优先。
    # #R39 bug/medium：清理仅非 dry-run 分支执行——dry-run 不重写该文件却先删除它，
    # 会清空上次失败批次已落盘的 id 清单（运维先 dry-run 验证修复会丢失"定点重跑"清单）。
    # #R42 bug/medium：清理必须**在 preflight 之后**——preflight 失败（服务不可达/
    # 维度不匹配）时 sys.exit 发生在清理前，上次运行的失败清单得以保留（否则 preflight
    # 失败 = 丢失定点重跑清单 = 被迫全量 --all 再付几千次付费调用）。
    failed_ids_path = os.path.join(HERE, "hype_failed_ids.txt")
    # #R39 performance/medium：嵌入服务预检——系统性配置错误（local 模型 768d 触发
    # dim≠RUST_DIM、服务不可达）若不先探测，--all 会先为每条付一次付费 LLM chat 再
    # 全部失败。循环前单次探测嵌入校验 dim==RUST_DIM，配置错误立即 fail-fast。
    if not args.dry_run:
        try:
            _vecs, pre_dim = embed(["preflight"])
        except Exception as e:
            print(f"错误: embed_server 不可达（{e}）——请先启动 embed_server 再补嵌")
            sys.exit(1)
        if pre_dim != RUST_DIM:
            print(f"错误: embed_server 维度 {pre_dim}≠Rust DIM {RUST_DIM}（模型不匹配，无法补嵌）")
            sys.exit(1)
        print(f"预检通过: embed_server dim={pre_dim} == RUST_DIM")
        # #R44 bug/medium：chat 端点同样预检——generate_question 对确定性 4xx（401 无效
        # key / 400 未知模型）立即 raise（见其 #R41 注释），但主循环的 catch-all except
        # 会把该 RuntimeError 重新归类为"重试耗尽瞬时故障"、记入 fail_ids：坏 key/未知
        # 模型会跑完全部目标、把每个 id 写进重跑清单、报告"失败 N"而非 fail-fast——
        # 直接违反"fail_ids 只含可修复瞬时故障"的契约。探测一次（几厘付费）即可在
        # 循环前暴露配置错误。
        try:
            _probe = generate_question("预检：请返回一句可检索的示例问题。")
        except Exception as e:
            print(f"错误: chat 端点预检失败（{e}）——请检查 SILICONFLOW_API_KEY/模型配置")
            sys.exit(1)
        # #R55 bug/low：探针必须返回非空内容——模型系统性拒答/恒空（错模型名/
        # 内容过滤常开）不抛异常，预检"通过"后 --all 每记忆付一次调用、全行确定性
        # skip（写入 0 / 跳过 N），fail-fast-before-spending 目标落空。
        if not _probe or len(_probe) < 6:
            print("错误: chat 端点预检返回空/过短内容（模型拒绝或配置错误）——请检查 MEMORIA_HYDE_MODEL")
            sys.exit(1)
        print("预检通过: chat 端点可访问")
        # preflight 通过后才清理旧清单（见上方 #R42 说明）。
        # #R46 bug/medium：清理仅限**全量**运行（--limit 为 None）——`--limit N` 是
        # 调试/抽样运行（argparse help 注明），只处理目标子集；无条件清理会让部分
        # 运行静默抹掉上次全量运行累积的失败清单（"定点重跑"工件），运维先 --limit
        # 调试再定点重跑时清单已丢，被迫全量 --all 再付几千次付费调用。dry-run 与
        # preflight 失败已有守卫，部分运行此前漏网。
        if args.all and args.limit is None:
            try:
                if os.path.exists(failed_ids_path):
                    os.remove(failed_ids_path)
            except OSError as e:
                print(f"  [warn] 清理旧 hype_failed_ids.txt 失败: {e}")

    # connect + DDL 移到 preflight 之后（#R50 maintainability/low）：此前 DDL 在
    # preflight 前执行——preflight 失败（服务不可达/bad key）sys.exit 时
    # memory_hype_vectors 已被创建（失败副作用，违背"友好诊断 / 失败无副作用"纪律），
    # 且只读 DB/磁盘满会裸抛 OperationalError traceback。preflight 全部通过后才
    # 触碰 DB（#R36 bug/medium：dry-run 仍不触碰——connect 会创建不存在的 DB 文件、
    # CREATE TABLE/INDEX 会真实执行，与"完成(dry-run，未写库)"文案矛盾）。
    con = None
    if not args.dry_run:
        # #R43 bug/medium：**golden 路径**同样需要存在性检查——MEMORIA_DB_PATH/--db
        # 缺失时 sqlite3.connect 静默创建空 DB，随后为 golden 记忆写入向量（memories
        # 无对应行），报告"写入 N / 失败 0"假成功；孤儿行在下次启动被 Rust 清理，
        # 整个运行是付费 no-op。镜像 --all 路径的诊断。
        if not os.path.exists(args.db):
            print(f"错误: 数据库不存在 {args.db}（请检查 MEMORIA_DB_PATH / --db）")
            sys.exit(1)
        # #R50 maintainability/low：DDL 包 try/except 友好诊断（只读/磁盘满/损坏）。
        try:
            con = sqlite3.connect(args.db)
            # #R58 bug/low：**DDL 前探测 memories 表**（golden 模式）——库文件存在但
            # 未初始化（无 memories 表）时，后续 golden 校验 SELECT 失败退出，但若
            # DDL 已执行则 memory_hype_vectors 与其索引被留在失败路径上（写副作用，
            # 违背"失败无副作用"纪律）。connect 后先查 memories 存在性，未初始化
            # 即友好退出（DDL 尚未执行，无副作用）。
            try:
                has_memories = con.execute(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories'"
                ).fetchone()[0]
            except sqlite3.Error:
                has_memories = 0
            if not has_memories:
                con.close()
                print(f"错误: {args.db} 未初始化（无 memories 表，请先启动服务或导入 schema）")
                sys.exit(1)
            # 自包含：表可能尚未经 Rust 迁移创建（离线补嵌先于服务启动时）——镜像
            # src/storage/sqlite.rs 的 schema，确保首次运行不因缺表崩溃、幂等成立。
            con.execute(
                "CREATE TABLE IF NOT EXISTS memory_hype_vectors ("
                "id TEXT PRIMARY KEY, namespace TEXT NOT NULL DEFAULT 'default', "
                "question TEXT, vector BLOB NOT NULL, updated_at TEXT DEFAULT (datetime('now')))"
            )
            con.execute(
                "CREATE INDEX IF NOT EXISTS idx_hype_ns ON memory_hype_vectors(namespace)"
            )
        except sqlite3.Error as e:
            if con is not None:
                con.close()
            print(f"错误: 打开/初始化 {args.db} 失败（只读/磁盘满/损坏）: {e}")
            sys.exit(1)

    # #R45 bug/low：**golden 路径**预检 id 存在性/有效性——stale golden（id 已删/
    # 过期/superseded，`eval_hyde_recall.py --build` 之后数据变化）写入的是无对应
    # memories 行的孤儿，启动时被 Rust 清理，整次运行付费 no-op 却报"写入 N / 失败 0"
    # 假成功（#R43 注释识别的失败模式只查了 DB 文件存在，未查 id 有效性）。镜像 --all
    # 的 SQL 过滤做一次性预检，失效 id 计 skip 而非白付。
    if not args.dry_run and not args.all:
        gids = [mid for mid, _ in targets]
        valid_ids = set()
        # #R46 bug/medium：DB 文件存在但**未初始化**（无 memories 表）时裸抛
        # OperationalError traceback——且此时 memory_hype_vectors 已作为副作用创建，
        # 违背"友好诊断 / 失败无副作用"纪律。镜像 --all 分支的 try/except。
        try:
            for i in range(0, len(gids), 500):
                chunk = gids[i : i + 500]
                ph = ",".join("?" * len(chunk))
                rows = con.execute(
                    "SELECT id FROM memories WHERE id IN (" + ph + ") "
                    "AND namespace='agent/xujiayan' AND superseded_by IS NULL "
                    "AND (valid_to IS NULL OR valid_to='' OR valid_to > strftime('%Y-%m-%dT%H:%M:%S','now'))",
                    chunk,
                ).fetchall()
                valid_ids.update(r[0] for r in rows)
        except sqlite3.Error as e:
            con.close()
            print(f"错误: 读取 {args.db} 的 memories 表失败（库未初始化/路径错误/损坏）: {e}")
            sys.exit(1)
        before = len(targets)
        targets = [(m, c) for m, c in targets if m in valid_ids]
        stale = before - len(targets)
        if stale:
            print(f"提示: {stale} 条 golden id 已失效（删除/过期/superseded），计 skip 跳过")
            skip += stale

    def record_fail(mid):
        """失败 id **立即追加**落盘（#R40 bug/medium）：脚本中断（Ctrl+C/崩溃）时
        已累计的失败清单不丢——"可定点重跑"承诺依赖该文件，删除旧清单后若只在循环
        结束时写一次，中断即全丢。
        #R46 bug/medium：仅**全量**运行（--limit is None）落盘——--limit 调试/抽样
        运行的失败既不写盘也不清理，旧清单保持完整（不被部分运行污染/覆盖）。"""
        fail_ids.append(mid)
        if not args.dry_run and args.all and args.limit is None:
            try:
                with open(failed_ids_path, "a", encoding="utf-8") as f:
                    f.write(mid + "\n")
            except OSError as e:
                print(f"  [warn] 追加 {failed_ids_path} 失败: {e}")
    for i, (mid, content) in enumerate(targets, 1):
        try:
            q = generate_question(content)
        except DeterministicSkipError as e:
            # #R45 bug/medium：确定性失败（4xx 配置错误/内容过滤拒绝）重跑永远同样失败
            # ——计 skip（skip_ids 提示），不进 fail_ids，避免清单永不收敛。
            print(f"[{i}/{len(targets)}] {mid[:8]} 问句生成确定性失败，跳过: {e}")
            skip += 1
            skip_ids.append(mid)
            continue
        except Exception as e:
            # 重试耗尽的瞬时故障：记入 fail_ids（可定点重跑），非确定性 skip。
            print(f"[{i}/{len(targets)}] {mid[:8]} 问句生成瞬时失败: {e}")
            fail += 1
            record_fail(mid)
            continue
        if not q or len(q) < 6:
            print(f"[{i}/{len(targets)}] {mid[:8]} 问句生成失败/过短，跳过")
            skip += 1
            # #R37 maintainability/low：区分"瞬时失败"与"确定性跳过"——sys_prompt 明确
            # 指示 LLM 对不可检索内容（纯代码/乱码）返回 ""，这类 id 重跑永远同样跳过，
            # 记入 fail_ids 会让 hype_failed_ids.txt 永不收敛。跳过单独记 skip_ids
            # （仅提示用），fail_ids 只含真正可修复的瞬时失败。
            skip_ids.append(mid)
            continue
        # dry-run 前置：只生成问句、不嵌入不写库——文档承诺"只生成问句"就必须
        # 不依赖 embed_server 在线（否则服务抖动时 dry-run 报"嵌入失败"而非显示问句）。
        if args.dry_run:
            print(f"[{i}/{len(targets)}] {mid[:8]} [dry] 问句: {q[:60]}")
            ok += 1
            continue
        try:
            vecs, dim = embed([q])
            # #R50 bug/medium：空 embeddings 列表（v = vecs[0] 抛 IndexError）与
            # struct.pack 失败（非数值向量抛 struct.error）都是**确定性**畸形响应
            # ——显式转 DeterministicSkipError 计 skip；否则经 catch-all 归为瞬时
            # 失败记入 fail_ids（永不收敛 + 每轮白付 chat/embed）。
            try:
                v = vecs[0]
                blob = struct.pack(f"<{len(v)}f", *v)
            except (IndexError, KeyError, struct.error, TypeError, ValueError, OverflowError) as e:
                raise DeterministicSkipError(f"embed 响应向量畸形（{type(e).__name__}: {e}）") from e
            # 维度校验也在 try 内：畸形响应（非序列/非数值）抛异常 →
            # 按失败计数跳过而非中断整个 --all 批（否则 5339 条白跑且无 fail_ids）。
            # 同时校验 embed_server 的 dim 与 Rust 侧 DIM 一致：服务器若跑 local 模型
            # （768d），len(v)==dim 会通过但 rebuild 侧全部静默跳过——必须显式拒绝。
            # #R49 bug/medium：维度/向量校验失败是**确定性**的（模型/响应缺陷，重跑
            # 必然同样失败）——抛 DeterministicSkipError 计 skip（skip_ids），若抛
            # ValueError 会被 catch-all 归为瞬时失败记入 fail_ids：清单永不收敛且每轮
            # 重跑先白付一次 Qwen chat 调用。struct.pack 失败（非数值向量）同理。
            if dim != RUST_DIM:
                raise DeterministicSkipError(f"服务维度 {dim}≠Rust DIM {RUST_DIM}（模型不匹配）")
            if len(v) != dim:
                raise DeterministicSkipError(f"维度异常 {len(v)}≠{dim}")
            # #R52 bug/medium：写前镜像 Rust 写侧退化防御（persist.rs put_vector_into
            # 拒绝 !finite || ==0 的向量）——脚本直写 SQL 绕过该防御：embed_server 对
            # 未返回项有零向量 fallback（[0.0]*EMBED_DIM），NaN 可经 Python json 的
            # NaN 扩展泄漏；此类向量过 len==dim 与 struct.pack 落库、报"写入 N /
            # 失败 0"假成功，但 rebuild_from_table 静默丢弃（skipped_degenerate），
            # 行永远不进 HyPE HNSW——付费 no-op 报成功。此处显式拒绝计确定性 skip。
            norm = sum(x * x for x in v)
            if norm != norm or norm == float("inf") or norm == 0.0:
                raise DeterministicSkipError("嵌入向量退化（零/NaN/Inf），跳过")
            # 写入也在此 try 内（#R35 bug/medium）：服务运行中 DB 可能被锁/磁盘满，
            # 单行写失败若抛到外层会中断整批且 fail_ids 永不落盘——违背
            # "不中断、可定点重跑"承诺。写失败按失败计数跳过并记录 id。
            con.execute(
                "INSERT INTO memory_hype_vectors (id, namespace, question, vector, updated_at) "
                "VALUES (?, 'agent/xujiayan', ?, ?, datetime('now')) "
                "ON CONFLICT(id) DO UPDATE SET question=excluded.question, vector=excluded.vector, "
                "namespace=excluded.namespace, updated_at=excluded.updated_at",
                (mid, q, blob),
            )
            pending_batch.append(mid)
            # #R54 performance/low：批量 commit（每 50 行 + 循环后最终一次）——--all
            # 5339 行逐行 commit 每次 fsync；ON CONFLICT DO UPDATE 幂等保证无正确性影响。
            if i % 50 == 0:
                con.commit()
                pending_batch.clear()
        except DeterministicSkipError as e:
            # #R45 bug/medium：embed 侧确定性失败（4xx 配置错误）计 skip 不进 fail_ids。
            print(f"[{i}/{len(targets)}] {mid[:8]} 嵌入确定性失败，跳过: {e}")
            skip += 1
            skip_ids.append(mid)
            continue
        except sqlite3.Error as e:
            # #R60 bug/medium：**rollback 清中止事务态**——失败 DML/COMMIT 让
            # 隐式事务进入 aborted 态，不 rollback 则后续每行 execute 持续失败
            # （一次 BUSY/磁盘满级联成整批失败，每行各付一次 chat 调用）；
            # 同时本批已 execute 未 commit 的行被丢弃，补记 fail_ids 防静默丢失。
            try:
                con.rollback()
            except sqlite3.Error:
                pass
            print(f"[{i}/{len(targets)}] {mid[:8]} 写入失败: {e}")
            record_fail(mid)
            # #R63 bug/low：**ok/fail 计数精确化**——两个失败模式分别处理：
            # (1) INSERT 失败：当前行不在 pending、ok 未 +1 → fail 只加
            #     len(pending)（此前少计 1）；
            # (2) 批 commit 失败：当前行在 pending、ok += 1 未执行 → ok 恢复该行
            #     （此前多减 1）。公式：ok -= len(pending) - (1 if 当前行在 pending)，
            #     fail += len(pending) + (0 if 当前行在 pending)。
            in_pending = mid in pending_batch
            pending_ok = len(pending_batch)
            ok = max(0, ok - pending_ok + (1 if in_pending else 0))
            fail += pending_ok + (0 if in_pending else 1)
            for pid in pending_batch:
                record_fail(pid)
            pending_batch.clear()
            continue
        except Exception as e:
            print(f"[{i}/{len(targets)}] {mid[:8]} 嵌入/写入异常: {e}")
            fail += 1
            record_fail(mid)
            continue
        ok += 1
        if i % 10 == 0:
            print(f"  ...{i}/{len(targets)}")
    # #R54 performance/low：循环结束最终 commit（最后一批不足 50 行的部分）。
    # #R58 bug/medium：**守卫**——此前逐行写错误被捕获后连接留在失败事务态
    # （未 rollback）或并发服务持锁时，con.commit() 裸抛 sqlite3.OperationalError
    # traceback，死在摘要打印与 hype_failed_ids.txt 原子重写之前（重跑清单丢失）。
    # rollback 清失败态使连接可复用/安全关闭。
    if not args.dry_run and con is not None:
        try:
            con.commit()
            pending_batch.clear()
        except sqlite3.Error as e:
            try:
                con.rollback()
            except sqlite3.Error:
                pass
            # #R61 bug/medium：**最终 commit 失败补记 pending**——最后一批（<50 行）
            # 已 execute 未 commit 的行被回滚丢弃：此前既不在库也不在 fail_ids
            # （静默丢失 + 假成功）；镜像行级 handler，补记 + ok 修正。
            for pid in pending_batch:
                record_fail(pid)
            if pending_batch:
                ok = max(0, ok - len(pending_batch))
                fail += len(pending_batch)
            pending_batch.clear()
            print(f"  [warn] 最终 commit 失败: {e}")

    if con is not None:
        con.close()
    if args.dry_run:
        # dry-run 不落库——文案须如实反映，否则运维误以为已写入（#R34 other/low）
        print(f"\n完成(dry-run，未写库): 生成 {ok} / 跳过 {skip} / 失败 {fail}")
        return
    print(f"\n完成: 写入 {ok} / 跳过 {skip} / 失败 {fail}")
    if skip_ids:
        print(f"提示: {len(skip_ids)} 条确定性跳过（LLM 判为不可检索，重跑不会成功），未写入失败列表")
    if fail_ids:
        # 追加式已实时落盘；此处去重重写（追加过程中同 id 可能多次失败产生重复行，
        # 定点重跑清单须无重复）。#R44 bug/low：先写临时文件再 os.replace **原子替换**——
        # 直接 'w' 截断原文件后若写入中途失败（磁盘满/崩溃），record_fail 追加累积的
        # 清单被毁，中断安全承诺恰在要保护的错误路径上失效。原子替换保证目标文件
        # 要么是旧完整清单、要么是新完整清单，绝不半写。
        # #R46/#R48 bug/medium：仅**全量 --all**（不带 --limit）运行重写文件——--limit
        # 调试运行与默认 golden 运行的失败集都是子集，覆盖/清理旧清单会破坏
        # "--all 全量累积的定点重跑工件"（#R48：golden 默认运行此前也满足
        # `limit is None` 门控，会静默销毁全量清单）。
        dedup = list(dict.fromkeys(fail_ids))
        if args.all and args.limit is None:
            try:
                tmp = failed_ids_path + ".tmp"
                with open(tmp, "w", encoding="utf-8") as f:
                    f.write("\n".join(dedup))
                os.replace(tmp, failed_ids_path)
            except OSError as e:
                print(f"  [warn] 重写 {failed_ids_path} 失败（追加清单仍保留）: {e}")
            else:
                # #R48 bug/low：成功提示只在 os.replace 成功后打印——替换失败时目标
                # 文件仍是带重复的原始追加清单，与"已写入去重清单"表述矛盾、误导运维
                # 误信文件已原子替换。
                print(f"失败 id 已写入 {failed_ids_path}（{len(dedup)} 条）")
        else:
            print(f"（非全量 --all 运行，未写 fail_ids 文件）本次失败 {len(dedup)} 条")


if __name__ == "__main__":
    main()
