#!/bin/bash
# setup-dual-remote.sh - 配置 GitHub + Gitee 双远端推送
# 用法: bash setup-dual-remote.sh   （在仓库根目录执行）
# 验证: git remote -v | grep push
set -euo pipefail
cd "$(dirname "$0")"
git config --unset-all remote.origin.pushurl 2>/dev/null || true
git remote set-url --add --push origin "git@github.com:HanqingTony/aBigfileTether.git"
git remote set-url --add --push origin "git@gitee.com:HanqingTony/aBigfileTether.git"
git remote -v
