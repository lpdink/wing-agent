#!/usr/bin/env bash
# devbox 入口：git 身份 → 首启 clone 工作仓库 → 启动工具宿主。
set -euo pipefail

# git 身份（提交/建 PR 用）
if [ -n "${GIT_USER_NAME:-}" ]; then
  git config --global user.name "$GIT_USER_NAME"
fi
if [ -n "${GIT_USER_EMAIL:-}" ]; then
  git config --global user.email "$GIT_USER_EMAIL"
fi

WORKSPACE="${WING_WORKSPACE:-/workspace/wing-agent}"
REPO="${WING_REPO:-lpdink/wing-agent}"

# 首次启动：clone 工作仓库（持久卷上没有则跳过——Agent 自己 git pull 更新）
if [ ! -d "$WORKSPACE/.git" ]; then
  echo "cloning $REPO -> $WORKSPACE"
  mkdir -p "$(dirname "$WORKSPACE")"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    git clone "https://x-access-token:${GITHUB_TOKEN}@github.com/${REPO}.git" "$WORKSPACE"
  else
    git clone "https://github.com/${REPO}.git" "$WORKSPACE"
  fi
fi

exec /app/.venv/bin/python /opt/toolhost.py
