#!/bin/bash
# Development run: debug build with verbose logs. The UI is served from web/ on disk,
# so edits to web/ show up on reload without rebuilding.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/daouchi-target}"
if [ -z "${ORT_DYLIB_PATH:-}" ] && [ -e "$HOME/.daouchi/lib/libonnxruntime.dylib" ]; then
    export ORT_DYLIB_PATH="$HOME/.daouchi/lib/libonnxruntime.dylib"
fi
export DAOUCHI_WEB_DIR="$SCRIPT_DIR/web"

RUST_LOG="${RUST_LOG:-debug}" exec cargo run -p daouchi-desktop
