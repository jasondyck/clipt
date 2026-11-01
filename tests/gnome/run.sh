#!/bin/bash
# End-to-end tests for clipt under headless GNOME Shell (portal capture,
# fullscreen xdg_toplevel overlay, wl_data_device clipboard).
# Started via gstart.sh (system bus + mocked logind + session bus).
set -u
LAYOUT=${1:-single}   # single | dual (1x + 2x side by side)
mkdir -p $XDG_RUNTIME_DIR && chmod 700 $XDG_RUNTIME_DIR
export HOME=/tmp/home; mkdir -p $HOME/Pictures
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
pastesize() { wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null; }

# GLib only accepts the desktop entry if Exec resolves; the portal needs the
# entry to register the "clipt" app ID.
ln -sf /clipt /usr/local/bin/clipt; cp /clipt.desktop /usr/share/applications/
# PipeWire, for screen recording (Mutter's screencasts go out over it).
pipewire >/tmp/pw.log 2>&1 & sleep 0.5; wireplumber >/tmp/wp.log 2>&1 &
MON="--virtual-monitor 1280x720"
[ "$LAYOUT" = dual ] && MON="$MON --virtual-monitor 1920x1080"
gnome-shell --headless --wayland --no-x11 $MON >/tmp/gs.log 2>&1 &
for _ in $(seq 100); do [ -S $XDG_RUNTIME_DIR/wayland-0 ] && break; sleep 0.1; done
sleep 4
export WAYLAND_DISPLAY=wayland-0
[ "$LAYOUT" = dual ] && python3 /t/set_scale.py 2 | sed 's/^/  /' && sleep 2
echo "== GNOME $(gnome-shell --version | awk '{print $3}') ($LAYOUT)"
clipt list | sed 's/^/  /'

echo "-- portal permission"
out=$(timeout 10 clipt full -o /tmp/f0.png 2>&1); rc=$?
check "without permission: fails fast instead of hanging (rc=$rc)" '[ $rc -ne 124 ]'
echo "  message: $(echo "$out" | tail -1 | sed 's/^ *//')"
gdbus call --session --dest org.freedesktop.impl.portal.PermissionStore --object-path /org/freedesktop/impl/portal/PermissionStore \
  --method org.freedesktop.impl.portal.PermissionStore.SetPermission screenshot true screenshot clipt "['yes']" >/dev/null
s=$(date +%s%N); clipt full -o /tmp/f.png; ms=$(( ($(date +%s%N)-s)/1000000 ))
want=1280x720; [ "$LAYOUT" = dual ] && want=4480x1440
check "with permission: captured $(size /tmp/f.png) in ${ms} ms" '[ "$(size /tmp/f.png)" = "$want" ]'
check "portal file cleaned up from ~/Pictures" '[ -z "$(ls $HOME/Pictures 2>/dev/null)" ]'

echo "-- recording (Mutter screencast over PipeWire)"
dur()   { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1" 2>/dev/null; }
vsize() { ffprobe -v error -select_streams v -show_entries stream=width,height -of csv=s=x:p=0 "$1" 2>/dev/null; }
clipt screen -o /tmp/rec-ref.png 2>/dev/null   # before anything opens
CLIPT_DEBUG=1 clipt record screen -o /tmp/rec.mp4 2>/tmp/rec.err & rp=$!
sleep 1.5; [ -x /testwin ] && { /testwin 20c040 400 300 & tw=$!; }; sleep 1.5
clipt record --stop; wait $rp; rc=$?
check "record screen exits cleanly (rc=$rc)" '[ $rc = 0 ]'
check "it went through Mutter's screencast" 'grep -q "Mutter screencast node" /tmp/rec.err'
d=$(dur /tmp/rec.mp4)
check "it lasts about 3 s (${d:-none})" 'awk "BEGIN{exit !(\"$d\"+0 > 2.0 && \"$d\"+0 < 3.6)}"'
want=1280x720
check "it's the first monitor at native size ($want, got $(vsize /tmp/rec.mp4))" '[ "$(vsize /tmp/rec.mp4)" = "$want" ]'
ffmpeg -v error -y -i /tmp/rec.mp4 -frames:v 1 /tmp/rec-first.png
d=$(magick compare -fuzz 12% -metric AE /tmp/rec-first.png <(magick /tmp/rec-ref.png -alpha off png:-) null: 2>&1 | cut -d" " -f1)
check "the first frame matches a screenshot (AE=$d)" '[ "${d%.*}" -lt 20000 ]'
if [ -n "${tw:-}" ]; then
  ffmpeg -v error -y -sseof -0.2 -i /tmp/rec.mp4 -frames:v 1 /tmp/rec-last.png
  g=$(magick /tmp/rec-last.png -fuzz 15% -fill white -opaque "#20c040" -fill black +opaque white -format "%[fx:mean]" info:)
  check "a window opened mid-recording shows at the end (green share $g)" 'awk "BEGIN{exit !($g > 0.05)}"'
  kill $tw 2>/dev/null
fi
grep -E "encoder:|recorded" /tmp/rec.err | sed 's/^clipt: /  /'
clipt record -g "100,50 301x200" -o /tmp/rec-g.mp4 2>/tmp/rec.err & rp=$!; sleep 1.5; clipt record --stop; wait $rp
check "record -g records the area (302x200, got $(vsize /tmp/rec-g.mp4))" '[ "$(vsize /tmp/rec-g.mp4)" = 302x200 ]'

mkfifo /tmp/rin; python3 /rdinput.py < /tmp/rin 2>/tmp/rd.err & exec 3>/tmp/rin
act() { for a in "$@"; do echo "$a" >&3; done; }
drag() { act "move $1 $2" down "move $(( ($1+$3)/2 )) $(( ($2+$4)/2 ))" "move $3 $4" up; sleep 0.5; }
gui() { CLIPT_DEBUG=1 clipt gui 2>/tmp/gui.err & sleep 1.5; }
sleep 0.5

if [ "$LAYOUT" = dual ]; then
  echo "-- dual: portal image sliced per monitor at native resolution"
  clipt full -o /tmp/df.png
  check "full desktop at the highest scale (4480x1440, got $(size /tmp/df.png))" '[ "$(size /tmp/df.png)" = 4480x1440 ]'
  clipt screen Meta-0 -o /tmp/d0.png; clipt screen Meta-1 -o /tmp/d1.png
  check "1x monitor at its native 1280x720 (got $(size /tmp/d0.png))" '[ "$(size /tmp/d0.png)" = 1280x720 ]'
  check "2x monitor at its native 1920x1080 (got $(size /tmp/d1.png))" '[ "$(size /tmp/d1.png)" = 1920x1080 ]'
  d=$(magick compare -metric AE <(magick /tmp/df.png -crop 1920x1080+2560+0 png:-) /tmp/d1.png null: 2>&1 | cut -d" " -f1)
  check "2x slice is pixel-identical to its part of the full image (AE=$d)" '[ "${d%.*}" -lt 100 ]'
fi

echo "-- overlay (xdg_toplevel fullscreen fallback)"
gui
check "overlay mapped and painted" 'grep -q "painted 100" /tmp/gui.err'
act "key Escape"; sleep 0.6
pgrep -f "clipt gui" >/dev/null && echo "  (first Esc after login went to the shell; closing via right-click/Esc)" && act "key Escape" && sleep 0.6
grep "key " /tmp/gui.err | sed 's/^/  /'
pkill -f "clipt gui"

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

if [ "$LAYOUT" = dual ]; then
  echo "-- dual: no monitor picker; drag straight onto the 2x monitor"
  gui
  check "one overlay per monitor ($(grep -c 'painted 100' /tmp/gui.err) painted)" '[ "$(grep -o "Meta-[01] buf0 painted 100" /tmp/gui.err | sort -u | wc -l)" = 2 ]'
  drag 1500 100 1800 300
  act "key Return"; sleep 1
  check "copy from the 2x monitor is 600x400 (got $(pastesize))" '[ "$(pastesize)" = 600x400 ]'
  gui
  act "move 600 400" click; sleep 0.4
  act "key Return"; sleep 1
  check "click on the 1x monitor copies its native 1280x720 (got $(pastesize))" '[ "$(pastesize)" = 1280x720 ]'
  gui
  drag 1180 200 1380 400
  act "key Return"; sleep 1
  p=$(pastesize)
  check "selection across the 1x/2x boundary: 200x200 at 1x or 400x400 at 2x (got $p)" '[ "$p" = 200x200 ] || [ "$p" = 400x400 ]'
fi

echo "-- Esc again, after the shell has settled"
echo marker | wl-copy; sleep 0.3
gui
drag 10 10 200 200
act "key Escape"; sleep 0.8
check "Esc closes it" '! pgrep -f "clipt gui" >/dev/null'
grep "key " /tmp/gui.err | sed 's/^/  /'
check "clipboard untouched" '[ "$(wl-paste -n 2>/dev/null)" = marker ]'

exec 3>&-
pkill -f "clipt gui"
echo "== gnome $LAYOUT: $pass passed, $fail failed"
[ $fail -eq 0 ]
