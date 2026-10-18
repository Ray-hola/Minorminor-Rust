#!/bin/sh
set -eu
cd -- "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export MACOSX_DEPLOYMENT_TARGET=12.0
if ! command -v cargo >/dev/null 2>&1; then
  echo '需要 Rust 稳定版。安装说明：https://rust-lang.org/tools/install/' >&2
  exit 1
fi
exec cargo run --locked --manifest-path native/Cargo.toml -- "$@"
