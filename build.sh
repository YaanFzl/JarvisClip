#!/usr/bin/env bash
# Build jarvis-clip.exe with the GNU Rust toolchain into dist/.
# Needs binutils (as, dlltool, windres) on PATH. Machine-specific settings
# (CARGO_HOME, TEMP, PATH) go in build.local.sh, which git ignores.
set -euo pipefail
cd "$(dirname "$0")"
[[ -f build.local.sh ]] && source build.local.sh
# Rust's own bundled linker, not whatever MinGW gcc is on PATH (WinLibs adds
# a default manifest that clashes with ours).
export RUSTFLAGS="${RUSTFLAGS:--C link-self-contained=yes}"
cargo build --release "$@"
mkdir -p dist
cp target/release/jarvis-clip.exe dist/
echo "dist/jarvis-clip.exe ready"
