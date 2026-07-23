# AGENTS.md — Operational Rules for AI Assistants (and humans)

This repository is **publicly open-sourced on GitHub**. Read this file **before running any `git`
command or editing code**. Violating these rules can leak private data, push to the wrong place,
or — most commonly — waste hours editing the wrong directory. All of these have happened before
and are now structurally prevented.

## Two local directories — know which is which (CRITICAL)
There are TWO local checkouts of `jiayan-xu/memoria`. Confusing them is the #1 cause of wasted work.

| Directory | Role | Edit code here? | Branch |
|-----------|------|-----------------|--------|
| `C:/Users/user/.qclaw/workspace/memoria-open` | **CANONICAL source of truth** (code + built binary) | ✅ YES — always | `main` (= `origin/main`) |
| `C:/Users/user/.qclaw/workspace/memoria` | **Runtime mirror** — holds the live DB (`data/memoria.db`, ~110k memories), `web/`, `.env`, and launcher scripts | ❌ NEVER | local `master` (mirrors `origin/main`) |

- The running `memoria-server.exe` is **always built from `memoria-open`**
  (`memoria-open/target/release/memoria-server.exe`). The watchdog/launcher
  (`memoria_stack_launcher.py`, `start_both_tray.ps1`) point the **binary** there; only the
  DB / `web/` / `.env` paths point at `memoria\`.
- **If you edit code in `memoria\`, your change will NOT reach the running binary** (it builds from
  `memoria-open`). This produces the classic "my edit didn't take effect" confusion. **Always edit
  `memoria-open`.**

## Canonical source of truth
- **GitHub repo:** `jiayan-xu/memoria` (default + only publish branch: **`main`**).
- **Canonical local checkout (edit & push from HERE):** `C:/Users/user/.qclaw/workspace/memoria-open`
- **Remote `origin`:** `https://ghfast.top/https://github.com/jiayan-xu/memoria.git`
  (the `ghfast.top/https://` prefix is a GitHub mirror proxy; treat it as `github.com/jiayan-xu/memoria`).

## Keeping the runtime mirror in sync
After you push changes to `memoria-open` (main), re-sync the runtime mirror so its checkout matches:
```sh
cd C:/Users/user/.qclaw/workspace/memoria
git fetch origin
git reset --hard origin/main      # data/, web/, .env are gitignored → safe; the DB is untouched
```
Do NOT `git pull` / `merge` there, and never commit in `memoria\` (a `pre-commit` hook blocks it).

## DO NOT push / edit from the runtime mirror
`memoria\` is guarded: it carries a `.NO_PUSH` marker and a `pre-push` hook that blocks pushes, plus a
`pre-commit` hook that blocks commits. If you (or another tool) somehow try, it is refused with a
message pointing you to `memoria-open`.

## Hard rules (P0)
1. **Before ANY `git push`:** confirm (a) `git remote -v` shows the canonical GitHub URL, and
   (b) the target branch is `main`. If unsure, STOP and ask the user.
2. **Never push to a branch other than `main`.** A `pre-push` hook enforces this mechanically.
3. **Never push secrets or private data.** No hardcoded API keys, tokens, passwords, or
   `C:/Users/<name>/...` absolute paths. Keep `.env` gitignored; read keys from env vars only.
4. **Rotate, don't commit.** If a secret must change, write it to `.env` (gitignored) or env vars —
   never into tracked files or commit messages.
5. A safety `pre-push` hook ships in `.githooks/pre-push`. After cloning, run
   `git config core.hooksPath .githooks` to activate it. It blocks wrong-branch, wrong-remote,
   branch-deletion, and `.NO_PUSH` checkouts.

## Build provenance (version carries git SHA)
`build.rs` injects `MEMORIA_BUILD_VERSION = "<pkg>-g<short-sha>[-dirty]"` at compile time
(e.g. `0.3.0-gdc43632`). It auto-refreshes on every commit (declared `rerun-if-changed` on the git
refs). The running `/health` endpoint and MCP `initialize` report this version, so you can always tell
which commit produced the running binary. No manual step required.

## Privacy history
On 2026-07-08 the repo was scrubbed: admin key rotated, agent API key rotated, hardcoded
`C:/Users/user/...` paths removed, internal review docs & runtime logs removed from the public tree.
Historical commits may still contain inert (revoked) secret strings — do not reintroduce live ones.

## The `/graph` endpoint is a CAPPED SAMPLE (do not treat it as the full graph)
`GET /graph?namespace=<ns>[&limit=N]` returns a **weight-biased subgraph preview**, NOT the whole
memory graph. Defaults: `limit=200` nodes, edges = `limit×3` (hard caps: 5000 nodes / 15000 edges).
This exists because the DB holds ~110k memories / ~54k relations — drawing all of them crashes the
browser (`vis.Network`).
- The JSON `summary.total_nodes` / `summary.total_edges` are the **displayed (capped)** counts.
- The REAL totals are in `summary.total_memories` / `summary.total_relations` (a separate `COUNT(*)`).
- If a user says "my graph only has 200 nodes / 500 edges", that is the cap, not the data size —
  point them at `total_memories` / `total_relations`, or raise `?limit=`.
- Editing graph caps lives in `src/web_api.rs` (`api_graph`): `node_cap` / `edge_cap`. After changing,
  rebuild (see build provenance) and restart.

