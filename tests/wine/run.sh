#!/bin/bash
# clipt.exe under Wine on Xvfb + Openbox, the editor driven by xdotool.
set -u
export HOME=/tmp/home WINEPREFIX=/tmp/home/.wine WINEDEBUG=-all
# No Mono or Gecko: their install prompts would wait forever.
export WINEDLLOVERRIDES="mscoree,mshtml="
mkdir -p $HOME /tmp/out
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
pastesize() { xclip -selection clipboard -t image/png -o 2>/dev/null | magick identify -format %wx%h - 2>/dev/null; }
drag() { xdotool mousemove $1 $2 mousedown 1 mousemove $(( ($1+$3)/2 )) $(( ($2+$4)/2 )) mousemove $3 $4 mouseup 1; sleep 0.5; }

Xvfb :1 -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
xsetroot -solid '#335577'; openbox >/dev/null 2>&1 &
timeout 120 wineboot -i >/dev/null 2>&1
C="timeout 30 wine /clipt.exe"
echo "== $(wine --version) on Xvfb 1600x1000"

out=$($C list)
check "list: one monitor at 1600x1000 ($out)" 'echo "$out" | grep -q "^1	1600x1000+0+0"'
$C full -o 'Z:\tmp\out\full.png'
check "full: $(size /tmp/out/full.png)" '[ "$(size /tmp/out/full.png)" = 1600x1000 ]'
$C screen 1 -o 'Z:\tmp\out\s1.png'
check "screen 1: $(size /tmp/out/s1.png)" '[ "$(size /tmp/out/s1.png)" = 1600x1000 ]'
$C full -g "10,20 300x200" -o 'Z:\tmp\out\g.png'
check "full -g: $(size /tmp/out/g.png)" '[ "$(size /tmp/out/g.png)" = 300x200 ]'

# Wine hands the Windows clipboard over to X while a Wine process runs.
wine notepad >/dev/null 2>&1 & sleep 3
$C full -c; sleep 1
check "full -c: a PNG on the clipboard ($(pastesize))" '[ "$(pastesize)" = 1600x1000 ]'
check "and a bitmap" 'xclip -selection clipboard -t TARGETS -o | grep -q image/bmp'

echo "-- the editor"
$C gui -o 'Z:\tmp\out\gui.png' & p=$!; sleep 4
drag 200 150 900 600
xdotool key r; sleep 0.2; drag 300 250 600 400
xdotool key t; xdotool mousemove 400 500 click 1; sleep 0.2; xdotool type "Hello"; xdotool key Return; sleep 0.5
magick import -window root /tmp/out/overlay.png
red=$(magick /tmp/out/overlay.png -crop 4x80+298+280 +repage -format '%[fx:mean.r-mean.g]' info:)
check "the rectangle shows on the overlay (red $red)" 'awk "BEGIN{exit !($red > 0.3)}"'
xdotool key ctrl+s; wait $p
check "Ctrl+S saves the selection ($(size /tmp/out/gui.png))" '[ "$(size /tmp/out/gui.png)" = 700x450 ]'
$C gui & p=$!; sleep 4
drag 100 100 420 340; xdotool key Return; wait $p; sleep 1
check "Enter copies it ($(pastesize))" '[ "$(pastesize)" = 320x240 ]'
$C gui & p=$!; sleep 4; xdotool key Escape; wait $p; rc=$?
check "Esc cancels (rc=$rc)" '[ $rc = 0 ]'
# Notepad opens at 0,0, 950x712 (with its frame).
$C gui & p=$!; sleep 4; xdotool mousemove 500 400 sleep 0.5 click 1; sleep 0.5; xdotool key ctrl+c; wait $p; sleep 1
check "a click picks the window under it ($(pastesize))" '[ "$(pastesize)" = 950x712 ]'

echo "-- the tray"
CLIPT_DEBUG=1 $C tray 2>/tmp/tray.err & tp=$!; sleep 3
check "clipt tray stays running" 'kill -0 $tp 2>/dev/null'
out=$($C tray 2>&1); rc=$?
check "a second tray says one is running (rc=$rc: ${out##*: })" '[ $rc != 0 ] && echo "$out" | grep -q "already running"'
# Print Screen, the default hotkey, starts the overlay.
xdotool key Print; sleep 4
check "Print Screen opens the overlay" 'pgrep -f "clipt.exe gui" >/dev/null'
xdotool key Escape; sleep 1
check "and Esc closes it" '! pgrep -f "clipt.exe gui" >/dev/null'
$C full -c; sleep 1
check "a screenshot tells the tray, which shows a notification" 'grep -q "notification: Screenshot copied" /tmp/tray.err'
# Quit, from the menu: the window's close does the same.
kill $tp; wait $tp 2>/dev/null

if [ -f /ffmpeg.exe ]; then
  echo "-- recording (ffmpeg.exe beside clipt.exe)"
  # Trying each GPU encoder costs Wine seconds apiece, and there's no GPU.
  $C config record_codec x264 >/dev/null
  R="timeout 180 wine /clipt.exe"
  started() { for _ in $(seq 900); do [ -s "$1" ] && return 0; sleep 0.1; done; return 1; }
  dims() { ffprobe -v error -select_streams v -show_entries stream=width,height -of csv=p=0:s=x "$1" 2>/dev/null; }
  secs() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1" 2>/dev/null; }
  CLIPT_DEBUG=1 $R record screen -o 'Z:\tmp\out\rec.mp4' 2>/tmp/rec.err & p=$!
  started /tmp/out/rec.mp4; sleep 2
  $R record --pause; pr=$?; sleep 3; $R record --resume; rr=$?; sleep 2
  $R record --stop; sr=$?; wait $p; rc=$?
  check "record screen, --pause, --resume, --stop (rc $pr $rr $sr $rc)" '[ "$pr$rr$sr$rc" = 0000 ]'
  check "a 1600x1000 MP4 ($(dims /tmp/out/rec.mp4))" '[ "$(dims /tmp/out/rec.mp4)" = 1600x1000 ]'
  # The writer's debug line: "recorded A s of B s".
  gap=$(sed -n 's/.*recorded \([0-9.]*\) s of \([0-9.]*\) s.*/\2 \1/p' /tmp/rec.err | awk '{printf "%.1f", $1 - $2}')
  check "the pause is left out (wall minus video: ${gap:-?} s)" "awk 'BEGIN{exit !($gap > 2.5 && $gap < 5)}'"
  $R record -g "10,20 301x200" -o 'Z:\tmp\out\g.mp4' & p=$!
  started /tmp/out/g.mp4; sleep 1; $R record --stop; wait $p
  check "record -g: padded to even ($(dims /tmp/out/g.mp4))" '[ "$(dims /tmp/out/g.mp4)" = 302x200 ]'
  $R record window notepad -o 'Z:\tmp\out\w.mp4' & p=$!
  started /tmp/out/w.mp4; xdotool search --name Notepad | head -1 | xargs -I{} xdotool windowmove {} 300 200; sleep 2
  $R record --stop; wait $p
  check "record window notepad ($(dims /tmp/out/w.mp4))" '[ "$(dims /tmp/out/w.mp4)" = 950x712 ]'
  $R record -o 'Z:\tmp\out\o.mp4' & p=$!; sleep 6
  drag 1000 500 1400 800; xdotool key Return
  started /tmp/out/o.mp4; sleep 1
  out=$($R record screen 2>&1); rc=$?
  check "a second recording is refused (rc=$rc: ${out##*: })" '[ $rc != 0 ] && echo "$out" | grep -q "already recording"'
  $R record --stop; wait $p
  check "the overlay's selection ($(dims /tmp/out/o.mp4))" '[ "$(dims /tmp/out/o.mp4)" = 400x300 ]'
  $R gui -o 'Z:\tmp\out\s.mp4' & p=$!; sleep 6
  drag 1000 500 1300 700; xdotool key ctrl+r; sleep 0.5; xdotool key Return
  started /tmp/out/s.mp4; sleep 1; $R record --stop; wait $p
  check "Ctrl+R in the screenshot overlay records ($(dims /tmp/out/s.mp4))" '[ "$(dims /tmp/out/s.mp4)" = 300x200 ]'
  $R record -g "0,0 200x100" --gif -o 'Z:\tmp\out\r.gif' & p=$!
  started '/tmp/out/.r.recording.mp4'; sleep 1; $R record --stop; wait $p
  check "--gif makes a GIF ($(size "/tmp/out/r.gif[0]"))" '[ "$(size "/tmp/out/r.gif[0]")" = 200x100 ]'
  # The tray follows a recording it didn't start, and hears when it's saved.
  CLIPT_DEBUG=1 $R tray 2>/tmp/tray2.err & tp=$!; sleep 3
  $R record -g "0,0 200x100" -o 'Z:\tmp\out\t.mp4' & p=$!
  started /tmp/out/t.mp4
  $R record --pause; sleep 2; $R record --stop; wait $p; sleep 2
  check "the tray's icon shows recording, then paused, then idle" \
    'sed -n "s/.*recording state: //p" /tmp/tray2.err | tr -d "\r" | tr "\n" " " | grep -q "recording paused idle"'
  check "and it shows \"Recording saved\"" 'grep -q "notification: Recording saved" /tmp/tray2.err'
  kill $tp; wait $tp 2>/dev/null

fi

echo "== $pass passed, $fail failed"
[ $fail = 0 ]
