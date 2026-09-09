#!/usr/bin/env bash
# Build the CLI and copy the binary to ./bin/asana in this checkout.
# The copy matters: several worktrees share one CARGO_TARGET_DIR and
# target/debug/asana belongs to whoever built last; ./bin/asana is yours.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
cargo build --quiet "$@"
mkdir -p "$ROOT/bin"
cp "$CARGO_TARGET_DIR/debug/asana" "$ROOT/bin/asana"
echo "built $ROOT/bin/asana"
