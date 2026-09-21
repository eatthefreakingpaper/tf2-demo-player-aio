#!/usr/bin/env bash
# One-command build + package: compiles the release exe and assembles the
# self-contained pack/ directory (exe + DLLs + glib schemas + pixbuf loaders).
#
# Works from Git Bash, MSYS2, or any other bash. Packaging needs MSYS2's
# UCRT64 environment for the GTK toolchain (pkg-config, ldd,
# glib-compile-schemas, gdk-pixbuf-query-loaders), so unless we are already
# inside a UCRT64 shell this re-executes itself through MSYS2's bash with the
# proper environment set up.

set -euo pipefail

MSYS2_DIR="${MSYS2_DIR:-/c/msys64}"
self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
script_dir="$(dirname "$self")"

if [[ "${MSYSTEM:-}" == "UCRT64" ]]; then
    cd "$script_dir"
    bash package_win.sh
else
    msys_bash="$MSYS2_DIR/usr/bin/bash.exe"
    if [[ ! -x "$msys_bash" ]]; then
        echo "error: MSYS2 not found at $MSYS2_DIR (set MSYS2_DIR to override)" >&2
        exit 1
    fi
    # Login shell (-l) applies MSYS2's profile for $MSYSTEM (PATH prefixes etc).
    MSYSTEM=UCRT64 "$msys_bash" -lc \
        'export PATH="$HOME/.cargo/bin:$PATH"; bash "$1"' _ "$self"
    # The re-executed copy prints the final message.
    exit 0
fi

echo
echo "pack/ ready at $script_dir/pack"
