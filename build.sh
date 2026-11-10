#!/usr/bin/env bash
# Build jarvis-clip.exe with the GNU Rust toolchain, keeping every cache on D:.
# Needs binutils (as, dlltool, windres) from MSYS2 on PATH; see README-jarvis.md.
set -euo pipefail
export CARGO_HOME='D:\ML\.cargo' TEMP='D:\ML\.tmp' TMP='D:\ML\.tmp'
export PATH="/d/ML/.tmp/binutils/mingw64/bin:$PATH"
# Rust's own bundled linker, not whatever MinGW gcc is on PATH (WinLibs adds
# a default manifest that clashes with ours).
export RUSTFLAGS="-C link-self-contained=yes"
cd "$(dirname "$0")"
/c/Users/User/.cargo/bin/cargo.exe build --release "$@"
mkdir -p dist
cp target/release/jarvis-clip.exe dist/
echo "dist/jarvis-clip.exe ready"
