# AGENTS.md — Operational Rules for AI Assistants (and humans)

This repository is **publicly open-sourced on GitHub**. Read this file **before running any `git`
command**. Violating these rules can leak private data or push to the wrong place — both have
happened here before and are now structurally prevented.

## Canonical source of truth
- **GitHub repo:** `jiayan-xu/memoria` (default branch: **`main`** — the ONLY branch)
- **Canonical local checkout (edit & push from HERE):** `C:/Users/user/.qclaw/workspace/memoria-open`
- **Remote `origin`:** `https://ghfast.top/https://github.com/jiayan-xu/memoria.git`
  - The `ghfast.top/https://` prefix is a GitHub mirror proxy. Treat it as `github.com/jiayan-xu/memoria`.

## DO NOT push from the other local copy
There is a SECOND, stale local working copy at `C:/Users/user/.qclaw/workspace/memoria`.
It is marked with a `.NO_PUSH` file and its `pre-push` hook blocks all pushes. Do not edit or push
from there — it previously held internal-only files (runtime logs, review docs) that must stay out of
the public repo. The public branch is `main` only; the old `master` branch was intentionally removed.

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

## Privacy history
On 2026-07-08 the repo was scrubbed: admin key rotated, agent API key rotated, hardcoded
`C:/Users/user/...` paths removed, internal review docs & runtime logs removed from the public tree.
Historical commits may still contain inert (revoked) secret strings — do not reintroduce live ones.
