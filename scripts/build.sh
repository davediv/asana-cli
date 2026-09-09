#!/usr/bin/env bash
# Build the CLI and copy the binary to ./bin/asana in this checkout.
# Set CARGO_TARGET_DIR per checkout when several worktrees build at once:
# cargo fingerprints the root package relative to the workspace root, so a
# shared target dir makes one worktree's build look "fresh" to another.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
cargo build --quiet "$@"
mkdir -p "$ROOT/bin"
# Replace on a fresh inode: overwriting a mapped Mach-O in place makes macOS
# SIGKILL the next run of it.
rm -f "$ROOT/bin/asana"
cp "$CARGO_TARGET_DIR/debug/asana" "$ROOT/bin/asana"
echo "built $ROOT/bin/asana"
