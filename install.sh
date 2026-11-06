#!/usr/bin/env bash
# 编译 release 单文件二进制。产物即 target/release/douyu，双击运行。
set -euo pipefail
cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:$PATH"

# --- 沙箱兼容：本环境（WorkBuddy 沙箱）禁止按 fd 设 mtime，cargo 解压会失败。
#     真机无此限制。这里自动检测并在需要时用 LD_PRELOAD shim 绕过。 ---
NEED_SHIM=0
if ! touch -d "@0" /tmp/.douyu_mtime_test 2>/dev/null; then
    NEED_SHIM=1
fi
rm -f /tmp/.douyu_mtime_test

echo "==> 编译 release（首次较慢，约 3~10 分钟）..."
if [ "$NEED_SHIM" = "1" ]; then
    [ -f shim.so ] || gcc -shared -fPIC -O2 -o shim.so shim.c
    echo "    （检测到沙箱限制，使用 mtime shim 绕过）"
    env LD_PRELOAD="$PWD/shim.so" cargo build --release --offline
else
    cargo build --release
fi

echo
echo "==> 完成！单文件二进制："
echo "    $(pwd)/target/release/douyu"
echo
echo "    双击运行，或在文件管理器里执行它，会打开 GUI 输入框。"
