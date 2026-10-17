#!/usr/bin/env bash
# 建一个 worktree，并从模板克隆构建缓存 —— 让新工作区只付"差异"的磁盘，而不是一整套产物。
#
# 为什么要克隆：Rust 的构建产物是按路径独立的一整套。裸 `git worktree add` 之后，
# 首次构建会凭空造出整套 target/；而在支持写时复制的文件系统（APFS / btrfs / XFS）上，
# 克隆近乎免费，随后只需重编工作区自己的 crate（外部依赖的产物可直接复用）。
#
# 只搬 target/debug：那是构建缓存所在（deps / .fingerprint / build / incremental）。
# release、llvm-cov-target（本地不跑插桩覆盖率）与 perf（其下是嵌套 git worktree）
# 与日常构建无关，不搬。
#
# 不克隆 .venv：venv 里的脚本写死了绝对路径，搬过去不可用。Python 环境照常 uv sync。
#
# 用法：
#   scripts/dev/new-worktree.sh <name> [base-ref] [branch]
# 默认 base = origin/develop，branch = feature/<name>（分支已存在时直接挂上去）；
# 落点是 <主仓同级>/wing-worktree/<name>（可用 WING_WORKTREE_ROOT 覆盖）。
set -euo pipefail

NAME="${1:?usage: new-worktree.sh <name> [base-ref] [branch]}"
BASE="${2:-origin/develop}"
BRANCH="${3:-feature/$NAME}"

ROOT="$(git rev-parse --show-toplevel)"
TEMPLATE_TARGET="$ROOT/target"
WT_ROOT="${WING_WORKTREE_ROOT:-$(dirname "$ROOT")/wing-worktree}"
WT="$WT_ROOT/$NAME"

if [ -e "$WT" ]; then
  echo "❌ $WT 已存在" >&2
  exit 1
fi

mkdir -p "$WT_ROOT"

# 手工删过目录但没 deregister 的残留会让 `worktree add` 直接失败，先清一遍。
git -C "$ROOT" worktree prune

if git -C "$ROOT" show-ref --verify --quiet "refs/heads/$BRANCH"; then
  git -C "$ROOT" worktree add "$WT" "$BRANCH"
  echo "✅ worktree: ${WT}（复用已有分支 ${BRANCH}）"
else
  git -C "$ROOT" worktree add -b "$BRANCH" "$WT" "$BASE"
  echo "✅ worktree: ${WT}（branch $BRANCH ← ${BASE}）"
fi

seed_target() {
  local src="$TEMPLATE_TARGET/debug" dst="$WT/target/debug" err
  mkdir -p "$WT/target"

  if err=$(cp -c -R "$src" "$dst" 2>&1); then
    :
  elif cp --version 2>/dev/null | grep -q "GNU coreutils" &&
    err=$(cp -a --reflink=auto "$src" "$dst" 2>&1); then
    :
  else
    rm -rf "$WT/target"
    echo "⚠️  target 克隆失败（已回滚，首次构建会全量重编）：" >&2
    echo "$err" >&2
    return 0
  fi

  if [ -f "$TEMPLATE_TARGET/.rustc_info.json" ]; then
    cp "$TEMPLATE_TARGET/.rustc_info.json" "$WT/target/" 2>/dev/null || true
  fi
  echo "✅ 已克隆 target/debug（写时复制，物理占用≈0）"
}

if [ -d "$TEMPLATE_TARGET/debug" ]; then
  seed_target
else
  echo "ℹ️  主仓没有 target/debug，跳过克隆（首次构建会全量重编）"
fi

cat <<EOF

下一步：
  cd "$WT"
  uv sync              # Python 环境不能从主仓搬，需要重建
  cargo build

收尾（用完就还，别囤）：
  git -C "$ROOT" worktree remove --force "$WT"
EOF
