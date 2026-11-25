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
# clipt's GNOME Shell extension, for the window list. Settings go to a file:
# without dconf in the container GNOME Shell couldn't keep it enabled.
clipt gnome-extension >/dev/null
export GSETTINGS_BACKEND=keyfile
gsettings set org.gnome.shell enabled-extensions "['clipt-windows@jasondyck.github.io']"
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
magick /tmp/rec-ref.png -alpha off /tmp/cmp-ref.png
d=$(magick compare -fuzz 12% -metric AE /tmp/rec-first.png /tmp/cmp-ref.png null: 2>&1 | cut -d" " -f1)
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

echo "-- window list (clipt's GNOME Shell extension)"
check "the extension is active ($(gnome-extensions info clipt-windows@jasondyck.github.io | sed -n 's/^ *State: //p'))" \
  'gnome-extensions info clipt-windows@jasondyck.github.io | grep -q "State: ACTIVE"'
if [ -x /testwin ]; then
  /testwin 20c040 400 300 & tw=$!; sleep 1.5
  out=$(gdbus call --session --dest io.github.jasondyck.clipt.Windows --object-path /io/github/jasondyck/clipt/Windows \
    --method io.github.jasondyck.clipt.Windows.List 2>&1)
  check "it answers only clipt (${out##*: })" 'echo "$out" | grep -q AccessDenied'
  CLIPT_DEBUG=1 clipt record window clipt-testwin -o /tmp/rec-w.mp4 2>/tmp/rec.err & rp=$!; sleep 1.5; clipt record --stop; wait $rp
  check "clipt finds the window through it and records it (400x300, got $(vsize /tmp/rec-w.mp4))" '[ "$(vsize /tmp/rec-w.mp4)" = 400x300 ]'
  grep -E "clipt:   " /tmp/rec.err | head -3 | sed 's/^clipt: /  /'
  kill $tw 2>/dev/null
fi

echo "-- recording frame (clipt's GNOME Shell extension)"
# A pixel in the frame's left strip, just outside the recorded area, in a
# screenshot taken while recording: red, amber while paused, gone after.
px()  { magick "$1" -format "%[fx:int(255*u.r)],%[fx:int(255*u.g)],%[fx:int(255*u.b)]" -crop 1x1+$2+$3 info: 2>/dev/null; }
near() { awk -v a="$1" -v b="$2" 'BEGIN{split(a,x,",");split(b,y,",");d=0;for(i=1;i<=3;i++){e=x[i]-y[i];d+=e<0?-e:e};exit !(d<60)}'; }
CLIPT_DEBUG=1 clipt record -g "100,50 301x200" -o /tmp/rec-f.mp4 2>/tmp/rec.err & rp=$!; sleep 1.5
clipt full -o /tmp/frame-on.png 2>/dev/null
clipt record --pause; sleep 0.5; clipt full -o /tmp/frame-paused.png 2>/dev/null
clipt record --stop; wait $rp; sleep 0.5
clipt full -o /tmp/frame-off.png 2>/dev/null
# The whole-desktop screenshot is at the highest monitor scale (2x in dual).
k=$(( $(magick identify -format %h /tmp/frame-on.png) / 720 )); fx=$((98 * k)); fy=$((150 * k))
on=$(px /tmp/frame-on.png $fx $fy); paused=$(px /tmp/frame-paused.png $fx $fy); off=$(px /tmp/frame-off.png $fx $fy)
check "a red frame just outside the area while recording ($on)" 'near "$on" 239,68,68'
check "amber while paused ($paused)" 'near "$paused" 245,158,11'
check "gone after the stop ($off)" '! near "$off" 239,68,68 && ! near "$off" 245,158,11'
check "and not in the video ($(vsize /tmp/rec-f.mp4))" '[ "$(vsize /tmp/rec-f.mp4)" = 302x200 ]'
grep -i "frame" /tmp/rec.err | head -2 | sed 's/^/  /'

mkfifo /tmp/rin; python3 /rdinput.py < /tmp/rin 2>/tmp/rd.err & exec 3>/tmp/rin

if [ "$LAYOUT" = single ]; then
# With one monitor GNOME's share dialog has it selected, so clicking Share
# answers it; with two, nothing is preselected and the dialog can come up
# inside the overview, which coordinate clicks can't drive reliably. The
# portal code is the same either way; the dual layout's Mutter recordings
# cover two monitors.
echo "-- recording through the ScreenCast portal (-b portal)"
# The first time, GNOME asks which screen to share; answer by clicking Share.
CLIPT_DEBUG=1 clipt record screen -b portal -o /tmp/portal.mp4 2>/tmp/rec.err & rp=$!
sleep 3; echo "move 925 148" >&3; sleep 0.3; echo click >&3; sleep 2.5
clipt record --stop 2>/dev/null; wait $rp; rc=$?
check "portal recording after Share exits cleanly (rc=$rc)" '[ $rc = 0 ]'
check "it went through the portal" 'grep -q "portal screencast node" /tmp/rec.err'
check "it's the first monitor at native size (1280x720, got $(vsize /tmp/portal.mp4))" '[ "$(vsize /tmp/portal.mp4)" = 1280x720 ]'
check "the choice is remembered (restore token saved)" '[ -s $HOME/.local/state/clipt/screencast-restore-token ]'
# The second time there's no dialog: the restore token answers it.
clipt record -g "100,50 301x200" -b portal -o /tmp/portal-g.mp4 2>/tmp/rec.err & rp=$!
sleep 3; clipt record --stop 2>/dev/null; wait $rp
check "with the token, a region records without the dialog, cropped (302x200, got $(vsize /tmp/portal-g.mp4))" '[ "$(vsize /tmp/portal-g.mp4)" = 302x200 ]'
fi
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
  magick /tmp/df.png -crop 1920x1080+2560+0 /tmp/cmp-a.png
  d=$(magick compare -metric AE /tmp/cmp-a.png /tmp/d1.png null: 2>&1 | cut -d" " -f1)
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

# Sound: once per desktop, as it doesn't depend on the layout.
if [ "$LAYOUT" = single ]; then
  . /audio.sh
  audio_tests "100,50 301x200"
fi

exec 3>&-
pkill -f "clipt gui"
echo "== gnome $LAYOUT: $pass passed, $fail failed"
[ $fail -eq 0 ]
