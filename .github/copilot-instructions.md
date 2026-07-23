# Copilot Instructions — jiayan-xu/memoria

This is a **public open-source** repo. Before any `git` command or code edit, read `AGENTS.md` (repo root).
The single most important rule:

- **CANONICAL checkout = `C:/Users/user/.qclaw/workspace/memoria-open` (branch `main`). Edit & push code ONLY here.**
- `C:/Users/user/.qclaw/workspace/memoria` is a RUNTIME MIRROR (live DB / `.env` / web / launchers).
  Editing code there will NOT affect the running binary (built from `memoria-open`) → wasted work. Never edit it.
- Before any `git push`: confirm `git remote -v` is the GitHub URL and target branch is `main`.
- Never commit secrets or `C:/Users/<name>/...` absolute paths. Keep `.env` gitignored; use env vars.
- `pre-push` hook (`.githooks/pre-push`) blocks wrong-branch / wrong-remote / deletion pushes;
  activate with `git config core.hooksPath .githooks` after cloning.
