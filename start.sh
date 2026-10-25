#!/bin/bash
# Build (release) and launch the Daouchi desktop app.
# Data, models and caches live in ~/.daouchi (override with DAOUCHI_HOME).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Keep build output on the internal disk; the project drive may be exFAT with large blocks.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/daouchi-target}"

# ONNX Runtime 1.22.x shared library. If ORT_DYLIB_PATH is unset and the library is in
# ~/.daouchi/lib, use it (the app also looks next to the binary and in <bundle>/Frameworks).
if [ -z "${ORT_DYLIB_PATH:-}" ] && [ -e "$HOME/.daouchi/lib/libonnxruntime.dylib" ]; then
    export ORT_DYLIB_PATH="$HOME/.daouchi/lib/libonnxruntime.dylib"
fi

echo "Daouchi  home: ${DAOUCHI_HOME:-$HOME/.daouchi}  build: $CARGO_TARGET_DIR"
exec cargo run --release -p daouchi-desktop
