#!/bin/zsh
set -eu
cd -- "$(dirname "$0")"
if [[ -d dist/Minorminor.app ]]; then
  exec /usr/bin/open "$PWD/dist/Minorminor.app"
fi
exec ./scripts/run-native.sh
