#!/usr/bin/env bash
# OpenCodeReview 门禁运行器 —— CI 与本地 git hook 共用
# 退出码: 0=成功(无发现 或 仅报告); 1=门禁触发(有发现且 --gate); 2=ocr 运行异常
#
# 用法:
#   ocr-review.sh [--gate] [--ocr-bin PATH] [ocr review 的其它参数...]
#   例: ocr-review.sh --gate --from origin/main --to HEAD
#   例: ocr-review.sh --from HEAD~3 --to HEAD
#
# 环境变量:
#   OCR_BIN   : 显式指定 ocr 二进制(默认: 优先 PATH 中的 ocr, 未找到则退化 npx; Windows 本机直跑 node 请经此变量或 --ocr-bin 指定)
#   OCR_GATE  : 设为 1 时等价于传 --gate
set -uo pipefail

OCR_BIN="${OCR_BIN:-}"
if [ -z "$OCR_BIN" ]; then
  if command -v ocr >/dev/null 2>&1; then
    OCR_BIN="ocr"
  else
    # 未安装到 PATH 时兜底 npx（联网拉取）。
    # Windows 本机直跑 node 的用法（避开 MSYS/Cygwin 对 /c/... 的路径转换坑）：
    #   OCR_BIN="<node.exe 绝对路径> <ocr.js 绝对路径>" 或 ocr-review.sh --ocr-bin "..."
    OCR_BIN="npx -y @alibaba-group/open-code-review"
  fi
fi

GATE=0
[ "${OCR_GATE:-0}" = "1" ] && GATE=1
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --gate) GATE=1; shift;;
    --ocr-bin) OCR_BIN="$2"; shift 2;;
    *) ARGS+=("$1"); shift;;
  esac
done

# 默认排除噪声（build 产物 / 备份 / 数据 / 缓存），避免无谓消耗 token
DEFAULT_EXCLUDE=".github/**,**/__pycache__/**,**/*.bak,**/*.exe,**/*.csv,**/target/**"
has_exclude=0
for a in "${ARGS[@]:-}"; do [ "$a" = "--exclude" ] && has_exclude=1; done
if [ "$has_exclude" = "0" ]; then ARGS+=("--exclude" "$DEFAULT_EXCLUDE"); fi

echo "[ocr] 运行: $OCR_BIN review ${ARGS[*]}"
OUT=$(eval "$OCR_BIN review ${ARGS[*]}" 2>&1)
RC=$?
echo "$OUT"
if [ $RC -ne 0 ]; then
  echo "[ocr] review 进程异常退出(rc=$RC)"; exit 2
fi

FINDINGS=$(echo "$OUT" | grep -oE '[0-9]+ finding\(s\)' | grep -oE '[0-9]+' | head -1)
FINDINGS=${FINDINGS:-0}
echo "[ocr] 评论数 = $FINDINGS"

if [ "$GATE" = "1" ] && [ "$FINDINGS" -gt 0 ]; then
  echo "[ocr] 门禁触发：发现 $FINDINGS 条评论，CI/提交被拦截。"
  echo "       处理后可 'git commit --no-verify' / 'git push --no-verify' 强制跳过(不推荐)。"
  exit 1
fi
exit 0
