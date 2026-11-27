#!/bin/bash
# MONITOR 桌面取景器启动脚本：首次运行自动构建（需要 Rust 工具链）。
set -e
cd "$(dirname "$0")"

export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
# 优先复用项目内的 crate 缓存
if [ -d ".rust/cargo/registry" ]; then
  export CARGO_HOME="${CARGO_HOME:-$PWD/.rust/cargo}"
fi

# 工具链探测：项目内置 .rust/ → 系统 cargo → ~/.cargo → ~/.rustup 工具链
if ! command -v cargo >/dev/null 2>&1; then
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  else
    for tc in "$HOME"/.rustup/toolchains/*/bin; do
      if [ -x "$tc/cargo" ]; then
        export PATH="$tc:$PATH"
        break
      fi
    done
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "未找到 cargo。请先安装 Rust："
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo "或: brew install rust"
  read -r -p "按回车键退出…"
  exit 1
fi

if ! command -v ffmpeg >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/ffmpeg ] && [ ! -x /usr/local/bin/ffmpeg ]; then
  echo "提示：未找到 ffmpeg，取景/录像需要它：brew install ffmpeg"
fi

echo "构建 MONITOR（release）…"
cargo build --release

BIN="./target/release/monitor-desktop"
if [ ! -x "$BIN" ]; then
  echo "构建产物缺失，请检查上面的错误信息。"
  read -r -p "按回车键退出…"
  exit 1
fi

echo "启动 MONITOR…"
exec "$BIN"
