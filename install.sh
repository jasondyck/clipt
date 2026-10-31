#!/bin/sh
# Build and install glint for the current user (~/.local).
set -eu
cd "$(dirname "$0")"
cargo build --release
prefix="${PREFIX:-$HOME/.local}"
install -Dm755 target/release/glint "$prefix/bin/glint"
# KWin matches the desktop file's Exec against the caller's executable path.
sed "s|^Exec=glint|Exec=$prefix/bin/glint|" data/glint.desktop > /tmp/glint.desktop.$$
install -Dm644 /tmp/glint.desktop.$$ "$prefix/share/applications/glint.desktop"
rm -f /tmp/glint.desktop.$$
echo "installed $prefix/bin/glint"
