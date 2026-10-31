#!/bin/bash
# End-to-end tests for glint under headless GNOME Shell (portal capture,
# fullscreen xdg_toplevel overlay, wl_data_device clipboard).
# Started via gstart.sh (system bus + mocked logind + session bus).
set -u
mkdir -p $XDG_RUNTIME_DIR && chmod 700 $XDG_RUNTIME_DIR
export HOME=/tmp/home; mkdir -p $HOME/Pictures
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
pastesize() { wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null; }

# GLib only accepts the desktop entry if Exec resolves; the portal needs the
# entry to register the "glint" app ID.
ln -sf /glint /usr/local/bin/glint; cp /glint.desktop /usr/share/applications/
gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x720 >/tmp/gs.log 2>&1 &
for _ in $(seq 100); do [ -S $XDG_RUNTIME_DIR/wayland-0 ] && break; sleep 0.1; done
sleep 4
export WAYLAND_DISPLAY=wayland-0
echo "== GNOME $(gnome-shell --version | awk '{print $3}')"
glint list | sed 's/^/  /'

echo "-- portal permission"
out=$(timeout 10 glint full -o /tmp/f0.png 2>&1); rc=$?
check "without permission: fails fast instead of hanging (rc=$rc)" '[ $rc -ne 124 ]'
echo "  message: $(echo "$out" | tail -1 | sed 's/^ *//')"
gdbus call --session --dest org.freedesktop.impl.portal.PermissionStore --object-path /org/freedesktop/impl/portal/PermissionStore \
  --method org.freedesktop.impl.portal.PermissionStore.SetPermission screenshot true screenshot glint "['yes']" >/dev/null
s=$(date +%s%N); glint full -o /tmp/f.png; ms=$(( ($(date +%s%N)-s)/1000000 ))
check "with permission: captured $(size /tmp/f.png) in ${ms} ms" '[ "$(size /tmp/f.png)" = 1280x720 ]'
check "portal file cleaned up from ~/Pictures" '[ -z "$(ls $HOME/Pictures 2>/dev/null)" ]'

mkfifo /tmp/rin; python3 /rdinput.py < /tmp/rin 2>/tmp/rd.err & exec 3>/tmp/rin
act() { for a in "$@"; do echo "$a" >&3; done; }
drag() { act "move $1 $2" down "move $(( ($1+$3)/2 )) $(( ($2+$4)/2 ))" "move $3 $4" up; sleep 0.5; }
gui() { GLINT_DEBUG=1 glint gui 2>/tmp/gui.err & sleep 1.5; }
sleep 0.5

echo "-- overlay (xdg_toplevel fullscreen fallback)"
gui
check "overlay mapped and painted" 'grep -q "painted 100" /tmp/gui.err'
act "key Escape"; sleep 0.6
pgrep -f "glint gui" >/dev/null && echo "  (first Esc after login went to the shell; closing via right-click/Esc)" && act "key Escape" && sleep 0.6
grep "key " /tmp/gui.err | sed 's/^/  /'
pkill -f "glint gui"

echo "-- drag + Enter copies"
gui
drag 100 100 400 300
act "key Return"; sleep 1
check "copied 300x200 (got $(pastesize))" '[ "$(pastesize)" = 300x200 ]'
grep -vE "painted|captured|ms  |key |pointer|keyboard|seat|notification" /tmp/gui.err | sed 's/^/  stderr: /' | head -5

echo "-- rectangle + Ctrl+S"
gui
drag 50 50 450 350
act "key r"; drag 100 100 300 250
act "ctrl s"; sleep 1
f=$(ls $HOME/Pictures/Screenshots/*.png 2>/dev/null | head -1)
check "saved $(size "$f") file" '[ "$(size "$f")" = 400x300 ]'
check "Ctrl+S also copied" '[ "$(pastesize)" = 400x300 ]'

echo "-- Esc again, after the shell has settled"
echo marker | wl-copy; sleep 0.3
gui
drag 10 10 200 200
act "key Escape"; sleep 0.8
check "Esc closes it" '! pgrep -f "glint gui" >/dev/null'
grep "key " /tmp/gui.err | sed 's/^/  /'
check "clipboard untouched" '[ "$(wl-paste -n 2>/dev/null)" = marker ]'

exec 3>&-
pkill -f "glint gui"
echo "== gnome: $pass passed, $fail failed"
[ $fail -eq 0 ]
