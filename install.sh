#!/bin/sh
# Build and install clipt for the current user (~/.local).
set -eu
cd "$(dirname "$0")"
prefix="${PREFIX:-$HOME/.local}"
# Release tarballs ship the binary next to this script; checkouts build it.
if [ -x ./clipt ] && [ ! -f Cargo.toml ]; then
  bin=./clipt
else
  cargo build --release
  bin=target/release/clipt
fi
install -Dm755 "$bin" "$prefix/bin/clipt"
# KWin matches the desktop file's Exec against the caller's executable path.
desktop=data/clipt.desktop
[ -f "$desktop" ] || desktop=clipt.desktop
sed "s|^Exec=clipt|Exec=$prefix/bin/clipt|" "$desktop" > /tmp/clipt.desktop.$$
install -Dm644 /tmp/clipt.desktop.$$ "$prefix/share/applications/clipt.desktop"
rm -f /tmp/clipt.desktop.$$
icon=data/clipt.svg
[ -f "$icon" ] || icon=clipt.svg
install -Dm644 "$icon" "$prefix/share/icons/hicolor/scalable/apps/clipt.svg"
# --tray: start `clipt tray` at login.
if [ "${1:-}" = "--tray" ]; then
  tray=data/clipt-tray.desktop
  [ -f "$tray" ] || tray=clipt-tray.desktop
  autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
  sed "s|^Exec=clipt|Exec=$prefix/bin/clipt|" "$tray" > /tmp/clipt-tray.desktop.$$
  install -Dm644 /tmp/clipt-tray.desktop.$$ "$autostart/clipt-tray.desktop"
  rm -f /tmp/clipt-tray.desktop.$$
  echo "clipt tray will start at login ($autostart/clipt-tray.desktop)"
fi
echo "installed $prefix/bin/clipt"
