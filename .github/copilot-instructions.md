# Copilot Instructions — jiayan-xu/memoria

This is a **public open-source** repo. Before any `git` command, follow the policy in `AGENTS.md`
(at repo root). Critical points:

- **Canonical local checkout:** `C:/Users/user/.qclaw/workspace/memoria-open`. **Default branch: `main`** (only branch).
- **DO NOT push from** `C:/Users/user/.qclaw/workspace/memoria` — it is a stale copy marked `.NO_PUSH`.
- Before any `git push`: confirm `git remote -v` is the GitHub URL and the target branch is `main`.
- Never commit secrets or `C:/Users/<name>/...` absolute paths. Keep `.env` gitignored; use env vars.
- A `pre-push` hook (`.githooks/pre-push`) blocks wrong-branch / wrong-remote / deletion pushes.
  Activate with `git config core.hooksPath .githooks` after cloning.
