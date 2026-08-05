#!/usr/bin/env bash
# 安装 git hooks（幂等、不破坏已有 hook）
# 将 scripts/pre-push 复制为 .git/hooks/pre-push（若已存在则跳过，避免覆盖）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/scripts/pre-push"
DST="$ROOT/.git/hooks/pre-push"
MARKER='# OpenCodeReview pre-push gate'

if [ -f "$DST" ] && grep -qF "$MARKER" "$DST"; then
  echo "[hooks] pre-push 已安装，跳过"; exit 0
fi

if [ -f "$DST" ]; then
  echo "[hooks] 已存在自定义 pre-push，备份为 $DST.bak 后安装 OCR 版"
  cp -f "$DST" "$DST.bak"
fi

cp -f "$SRC" "$DST"
chmod +x "$DST"
echo "[hooks] 已安装 pre-push -> $DST"
echo "       默认仅报告；设 OCR_GATE=1 启用拦截："
echo "       echo 'export OCR_GATE=1' >> ~/.bashrc"
