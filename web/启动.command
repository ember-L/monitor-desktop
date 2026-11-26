#!/bin/bash
# 双击运行：启动本地 HTTPS 服务器并打开浏览器
cd "$(dirname "$0")" || exit 1
if ! command -v python3 >/dev/null 2>&1; then
  osascript -e 'display alert "需要安装 Python 3" message "请先安装 Python 3（https://www.python.org）后重试。"'
  exit 1
fi
python3 serve.py --open
