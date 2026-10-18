#!/bin/bash
# Read-only policy adapter to the pinned, unmodified Mole source. No clean main,
# safe_remove, package-manager command, sudo, or deletion is invoked here.
set -euo pipefail
MINOR_TARGET="$1"
MINOR_SOURCE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../vendor/mole/bin" && pwd)/clean.sh"
source "$MINOR_SOURCE"
if should_protect_path "$MINOR_TARGET"; then
    echo 'Mole 保护此路径，文件已保留'; exit 1
fi
if is_path_whitelisted "$MINOR_TARGET"; then
    echo '命中 Mole 白名单，文件已保留'; exit 1
fi
if holds_compiled_model_cache "$MINOR_TARGET"; then
    echo '包含受保护的模型缓存，文件已保留'; exit 1
fi
_mole_reset_process_snapshot
if ! validate_path_for_deletion "$MINOR_TARGET"; then
    echo "Mole 安全检查未通过：${_MOLE_VALIDATION_REASON:-应用正在使用、路径受保护或无法确认状态}"; exit 1
fi
