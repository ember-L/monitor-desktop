#!/bin/bash
# MONITOR 桌面取景器启动脚本：首次运行自动构建（需要 Rust 工具链）。
set -e
cd "$(dirname "$0")"

# 工具链探测顺序：项目内置 .rust/（须已完整安装）→ 系统 cargo → ~/.rustup 工具链
if [ -x ".rust/cargo/bin/cargo" ] && ls .rust/rustup/toolchains/*/bin/rustc >/dev/null 2>&1; then
  export RUSTUP_HOME="$PWD/.rust/rustup"
  export CARGO_HOME="$PWD/.rust/cargo"
  export PATH="$PWD/.rust/cargo/bin:$PATH"
elif ! command -v cargo >/dev/null 2>&1; then
  for tc in "$HOME"/.rustup/toolchains/*/bin; do
    if [ -x "$tc/cargo" ]; then
      export PATH="$tc:$PATH"
      break
    fi
  done
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "未找到 cargo。请先安装 Rust："
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo "或: brew install rust"
  read -r -p "按回车键退出…"
  exit 1
fi

# 复用项目内的 crate 缓存（若存在），避免重新下载依赖
if [ -z "${CARGO_HOME:-}" ] && [ -d ".rust/cargo/registry" ]; then
  export CARGO_HOME="$PWD/.rust/cargo"
fi

if ! command -v ffmpeg >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/ffmpeg ] && [ ! -x /usr/local/bin/ffmpeg ]; then
  echo "提示：未找到 ffmpeg，取景/录像需要它：brew install ffmpeg"
fi

echo "构建 MONITOR（release）…"
cargo build --release

echo "启动 MONITOR…"
exec ./target/release/monitor-desktop
