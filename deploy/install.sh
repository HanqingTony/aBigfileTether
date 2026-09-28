#!/bin/bash
### install.sh - 构建并部署 tether（默认放进仓库工作区 .tether/，gitignored）
### 用法:
###   bash deploy/install.sh                              # 装到 <repo>/.tether/tether
###   bash deploy/install.sh --dest /usr/local/bin        # 装到系统 PATH
###   bash deploy/install.sh --host tony@192.168.0.101 --dest /home/tony/.tether
### 选项: --host <ssh目标> --dest <目录> --features <cargo特性> --no-build
### 验证: <dest>/tether --version
set -euo pipefail

REPO_DIR="$(cd -P "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
HOST=""
DEST="$REPO_DIR/.tether"
FEATURES=""
BUILD=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) HOST="${2:?}"; shift 2 ;;
        --dest) DEST="${2:?}"; shift 2 ;;
        --features) FEATURES="${2:?}"; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        -h|--help) grep '^### ' "$0" | sed 's/^### //'; exit 0 ;;
        *) echo "[ERROR] 未知参数: $1（用 -h 查看用法）"; exit 1 ;;
    esac
done

cd "$REPO_DIR"
if [[ "$BUILD" -eq 1 ]]; then
    echo "[INFO] 构建 release（features='${FEATURES}'）..."
    if [[ -n "$FEATURES" ]]; then
        cargo build --release --features "$FEATURES"
    else
        cargo build --release
    fi
fi

BIN="$REPO_DIR/target/release/tether"
[[ -x "$BIN" ]] || { echo "[ERROR] 未找到 $BIN（先去掉 --no-build 构建）"; exit 1; }
echo "[INFO] 版本: $("$BIN" --version)"

if [[ -n "$HOST" ]]; then
    echo "[INFO] 部署到 $HOST:$DEST/tether"
    ssh "$HOST" "mkdir -p '$DEST'"
    scp -q "$BIN" "$HOST:$DEST/tether"
    ssh "$HOST" "chmod 755 '$DEST/tether' && '$DEST/tether' --version"
    echo "[INFO] 若该目录不在对端【非登录】PATH，请在发起端设 [transfer] remote_bin=\"$DEST/tether\""
else
    echo "[INFO] 部署到本机 $DEST/tether"
    mkdir -p "$DEST" 2>/dev/null || sudo mkdir -p "$DEST"
    install -m755 "$BIN" "$DEST/tether" 2>/dev/null || sudo install -m755 "$BIN" "$DEST/tether"
    if command -v tether >/dev/null 2>&1; then
        echo "[INFO] PATH 命中: $(command -v tether)"
    else
        echo "[WARN] $DEST 不在 PATH；对等端建议用绝对路径做 remote_bin"
    fi
fi
