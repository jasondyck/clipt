#!/bin/bash
# End-to-end tests for clipt on X11 (Xvfb + Openbox). Usage: run.sh <layout>
#   single   one 1920x1080 monitor
#   multi    1920x1080 + 2560x1440 offset 200px down + 1280x1024 below, as
#            RandR 1.5 monitors on headless Xorg (dummy driver); Xvfb ignores
#            --setmonitor and stacks Xinerama screens at 0,0
set -u
LAYOUT=${1:-single}
export HOME=/tmp/home && mkdir -p $HOME
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
px()   { magick "$1" -format "%[pixel:p{$2}]" info: 2>/dev/null; }
paste_png() { xclip -selection clipboard -t image/png -o 2>/dev/null; }
pastesize() { paste_png | magick identify -format %wx%h - 2>/dev/null; }
ref() { magick import -window root -crop "$1" +repage "$2" 2>/dev/null; }   # reference capture
ae() { magick compare -metric AE "$1" "$2" null: 2>&1 | cut -d" " -f1; }

case $LAYOUT in
  single)
    SCREEN=1920x1080
    Xvfb :1 -screen 0 1920x1080x24 -nolisten tcp >/tmp/xvfb.log 2>&1 & ;;
  multi)
    SCREEN=4480x2464
    Xorg :1 -config /etc/X11/xorg-dummy.conf -noreset -nolisten tcp -logfile /tmp/xorg.log >/dev/null 2>&1 & ;;
esac
for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
if [ "$LAYOUT" = multi ]; then
  xrandr --output DUMMY0 --off
  xrandr --fb $SCREEN
  xrandr --setmonitor DP-1 1920/510x1080/290+0+0 none
  xrandr --setmonitor DP-2 2560/600x1440/340+1920+200 none
  xrandr --setmonitor HDMI-1 1280/340x1024/270+0+1080 none
fi
# Noisy wallpaper so the PNG is far bigger than one X request: exercises INCR.
W=${SCREEN%x*}; H=${SCREEN#*x}
magick -size ${W}x${H} gradient:'#d02020-#2040e0' \( -size ${W}x${H} xc: +noise Random -channel G -separate +channel \) \
  -compose overlay -composite -fill white -draw "rectangle 600,300 1300,700" /tmp/bg.png
magick display -window root /tmp/bg.png &
openbox >/tmp/openbox.log 2>&1 &
# wait until the wallpaper is really on the root window
for _ in $(seq 100); do
  [ "$(magick import -window root -crop 1x1+700+400 +repage -format '%[pixel:p{0,0}]' info: 2>/dev/null)" = "srgb(255,255,255)" ] && break
  sleep 0.2
done
sleep 0.5
echo "== x11 $LAYOUT ($(Xvfb -version 2>&1 | grep -i 'x.org x server' | head -1))"
/clipt list | sed 's/^/  /'

echo "-- headless capture vs ImageMagick import"
# `full` covers the monitors' bounding box (like grim), not the whole X
# framebuffer, which can be larger than what any monitor shows.
BBOX=$(/clipt list | grep -v backends | awk '{split($2,a,/[x+]/); r=a[3]+a[1]; b=a[4]+a[2]; if (r>R) R=r; if (b>B) B=b} END {print R "x" B}')
/clipt full -o /tmp/full.png; ref ${BBOX}+0+0 /tmp/ref.png
check "full is the monitors' bounding box ($BBOX, got $(size /tmp/full.png))" '[ "$(size /tmp/full.png)" = "$BBOX" ]'
for geo in $(/clipt list | grep -v backends | awk '{print $2}'); do
  d=$(magick compare -metric AE <(magick /tmp/full.png -crop $geo +repage png:-) <(magick /tmp/ref.png -crop $geo +repage png:-) null: 2>&1 | cut -d" " -f1)
  check "full matches import inside monitor $geo (AE=$d)" '[ "$d" = 0 ]'
done
for o in $(/clipt list | grep -v backends | cut -f1); do
  geo=$(/clipt list | awk -v o=$o '$1==o {print $2}')
  /clipt screen $o -o /tmp/s.png; ref "$geo" /tmp/r.png
  check "screen $o $(size /tmp/s.png) matches import (AE=$(ae /tmp/s.png /tmp/r.png))" '[ "$(size /tmp/s.png)" = "$(size /tmp/r.png)" ] && [ "$(ae /tmp/s.png /tmp/r.png)" = 0 ]'
done
/clipt full -g "100,50 300x200" -o /tmp/g.png; ref 300x200+100+50 /tmp/gr.png
check "geometry crop matches import (AE=$(ae /tmp/g.png /tmp/gr.png))" '[ "$(ae /tmp/g.png /tmp/gr.png)" = 0 ]'
/clipt full --cursor -o /tmp/cur.png
check "--cursor capture works" '[ "$(size /tmp/cur.png)" = "$BBOX" ]'

echo "-- recording (MIT-SHM + DAMAGE)"
dur()   { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1" 2>/dev/null; }
vsize() { ffprobe -v error -select_streams v -show_entries stream=width,height -of csv=s=x:p=0 "$1" 2>/dev/null; }
even()  { awk "BEGIN{w=$1; h=$2; printf \"%dx%d\", w+w%2, h+h%2}"; }
read O1 G1 < <(/clipt list | grep -v backends | head -1 | awk '{print $1, $2}')
ref "$G1" /tmp/rec-ref.png
CLIPT_DEBUG=1 /clipt record screen $O1 -o /tmp/rec.mp4 2>/tmp/rec.err & rp=$!
sleep 1.5; xsetroot -solid '#20c040'; sleep 1.5
/clipt record --stop; wait $rp; rc=$?
check "record screen exits cleanly (rc=$rc)" '[ $rc = 0 ]'
d=$(dur /tmp/rec.mp4)
check "it lasts about 3 s (${d:-none})" 'awk "BEGIN{exit !(\"$d\"+0 > 2.3 && \"$d\"+0 < 3.5)}"'
W1=${G1%%x*}; H1=${G1#*x}; H1=${H1%%+*}
check "it's $O1 at its size ($(even $W1 $H1), got $(vsize /tmp/rec.mp4))" '[ "$(vsize /tmp/rec.mp4)" = "$(even $W1 $H1)" ]'
ffmpeg -v error -y -i /tmp/rec.mp4 -frames:v 1 /tmp/rec-first.png
d=$(magick compare -fuzz 12% -metric AE /tmp/rec-first.png <(magick /tmp/rec-ref.png -alpha off png:-) null: 2>&1 | cut -d" " -f1)
check "the first frame matches import (AE=$d)" '[ "${d%.*}" -lt $((W1 * H1 / 50)) ]'
ffmpeg -v error -y -sseof -0.2 -i /tmp/rec.mp4 -frames:v 1 /tmp/rec-last.png
p=$(px /tmp/rec-last.png "$((W1 / 2)),$((H1 / 2))")
check "the root repainted mid-recording shows at the end ($p)" 'echo "$p" | grep -Eq "srgba?\(([0-9]|[1-5][0-9]),(1[6-9][0-9]|2[0-9][0-9]),([0-9]|[1-9][0-9])"'
grep -E "encoder:|recorded" /tmp/rec.err | sed 's/^clipt: /  /'
/clipt record -g "100,50 301x200" -o /tmp/rec-g.mp4 2>/dev/null & rp=$!; sleep 1.5; /clipt record --stop; wait $rp
check "record -g crops to the region (302x200, got $(vsize /tmp/rec-g.mp4))" '[ "$(vsize /tmp/rec-g.mp4)" = 302x200 ]'

echo "-- recording a window: followed by position, zoom eases on resize"
xlogo -geometry 400x300+150+150 & xl=$!; sleep 1
CLIPT_DEBUG=1 /clipt record window xlogo -o /tmp/rec-w.mp4 2>/tmp/rec.err & rp=$!; sleep 1.2
xdotool search --class xlogo windowsize --sync 800 300 2>/dev/null; sleep 1.2
xdotool search --class xlogo windowmove 500 400 2>/dev/null; sleep 1
/clipt record --stop; wait $rp; kill $xl 2>/dev/null
check "it follows the window by position" 'grep -q "following the X11 window" /tmp/rec.err'
read VW VH < <(vsize /tmp/rec-w.mp4 | tr x " ")
check "the video keeps the window's first size (${VW}x${VH}, about 400x300 plus the frame)" '[ "${VW:-0}" -ge 400 ] && [ "${VW:-0}" -le 440 ]'
heights() {
  rm -rf /tmp/cols && mkdir /tmp/cols
  ffmpeg -v error -i "$1" -vf "crop=2:ih:iw/2:0" -fps_mode passthrough /tmp/cols/%04d.png
  for f in /tmp/cols/*.png; do magick "$f" -fuzz 12% -transparent "#1e1e22" -trim -format "%h\n" info: 2>/dev/null; done
}
hs=$(heights /tmp/rec-w.mp4 | sort -un | tr "\n" " ")
check "the zoom eases through several sizes after the resize (heights: $hs)" '[ "$(echo $hs | wc -w)" -ge 4 ]'
magick display -window root /tmp/bg.png &
for _ in $(seq 100); do   # the wallpaper back, as later tests compare with earlier captures
  [ "$(magick import -window root -crop 1x1+700+400 +repage -format '%[pixel:p{0,0}]' info: 2>/dev/null)" = "srgb(255,255,255)" ] && break
  sleep 0.2
done
sleep 0.5

echo "-- headless clipboard (CLIPBOARD selection, INCR for big images)"
/clipt full -c; sleep 0.5
paste_png > /tmp/paste.png
echo "  png is $(stat -c %s /tmp/full.png) bytes"
check "xclip pastes the capture (AE=$(ae /tmp/paste.png /tmp/full.png))" '[ "$(ae /tmp/paste.png /tmp/full.png)" = 0 ]'
check "TARGETS lists image/png" 'xclip -selection clipboard -t TARGETS -o | grep -q image/png'
pid=$(pgrep -f "^/clipt full -c" | head -1)
[ -n "$pid" ] && echo "  clipboard daemon RSS: $(grep VmRSS /proc/$pid/status | awk '{print $2}') kB"
echo hello | xclip -selection clipboard; sleep 0.5
check "daemon exits when something else is copied" '! pgrep -f "^/clipt full -c" >/dev/null'

gui() { ${DEBUG:+env CLIPT_DEBUG=1} /clipt gui 2>/tmp/gui.err & sleep 1; }
drag() { xdotool mousemove $1 $2 mousedown 1 mousemove $(( ($1+$3)/2 )) $(( ($2+$4)/2 )) mousemove $3 $4 mouseup 1; sleep 0.3; }
key() { xdotool key "$@"; sleep 0.2; }
gerr() { grep -v "notification failed" /tmp/gui.err | grep -vE "ms  |captured|painted" | sed 's/^/  stderr: /'; }

echo "-- interactive: drag + Enter copies (over a running window manager)"
gui
drag 100 100 400 300; key Return; sleep 0.8
paste_png > /tmp/sel.png; ref 300x200+100+100 /tmp/selr.png
check "copied 300x200 (got $(size /tmp/sel.png)), pixels match (AE=$(ae /tmp/sel.png /tmp/selr.png))" '[ "$(ae /tmp/sel.png /tmp/selr.png)" = 0 ]'
pid=$(pgrep -f "^/clipt gui" | head -1)
[ -n "$pid" ] && echo "  gui clipboard daemon RSS: $(grep VmRSS /proc/$pid/status | awk '{print $2}') kB"
gerr

echo "-- interactive: rectangle + Ctrl+S"
gui
drag 50 50 450 350; key r; drag 100 100 300 250; key ctrl+s; sleep 0.8
f=$(ls $HOME/Pictures/Screenshots/*.png 2>/dev/null | head -1)
check "saved 400x300 (got $(size "$f"))" '[ "$(size "$f")" = 400x300 ]'
p=$(px "$f" 50,125)
check "red rectangle stroke ($p)" 'echo "$p" | grep -Eq "srgba?\((2[0-9][0-9]|1[6-9][0-9]),[0-9]{1,2},[0-9]{1,2}"'
check "also copied" '[ "$(pastesize)" = 400x300 ]'
gerr

echo "-- interactive: Esc cancels"
echo marker | xclip -selection clipboard; sleep 0.3
gui
drag 10 10 200 200; key Escape; sleep 0.5
check "exited" '! pgrep -f "^/clipt gui" >/dev/null'
check "clipboard untouched" '[ "$(xclip -selection clipboard -o 2>/dev/null)" = marker ]'

echo "-- interactive: text, undo, double-click"
gui
drag 100 100 500 400; key t; xdotool mousemove 150 150 click 1; sleep 0.2
xdotool type --delay 30 "Hi there"; key Return; key a; drag 200 300 400 200; key ctrl+z
key v; xdotool mousemove 300 250 click --repeat 2 --delay 80 1; sleep 0.8
check "double-click copied the 400x300 selection" '[ "$(pastesize)" = 400x300 ]'

if [ "$LAYOUT" = multi ]; then
  echo "-- multi: no monitor picker, every monitor has an overlay"
  gui
  check "one overlay window per monitor ($(xdotool search --onlyvisible --class clipt 2>/dev/null | wc -l))" '[ "$(xdotool search --onlyvisible --class clipt 2>/dev/null | wc -l)" = 3 ]'
  drag 2100 400 2500 700; key Return; sleep 0.8
  paste_png > /tmp/m2.png; ref 400x300+2100+400 /tmp/m2r.png
  check "drag on the second monitor copies 400x300 matching import (AE=$(ae /tmp/m2.png /tmp/m2r.png))" '[ "$(ae /tmp/m2.png /tmp/m2r.png)" = 0 ]'
  gui
  xdotool mousemove 500 1500 click 1; sleep 0.3; key Return; sleep 0.8
  paste_png > /tmp/m3.png; ref 1280x1024+0+1080 /tmp/m3r.png
  check "click on the monitor below copies its 1280x1024 matching import (got $(size /tmp/m3.png), AE=$(ae /tmp/m3.png /tmp/m3r.png))" '[ "$(ae /tmp/m3.png /tmp/m3r.png)" = 0 ]'
  gui
  drag 1800 300 2040 500; key Return; sleep 0.8
  paste_png > /tmp/mx.png; ref 240x200+1800+300 /tmp/mxr.png
  check "selection across monitors 1/2 matches import (AE=$(ae /tmp/mx.png /tmp/mxr.png))" '[ "$(ae /tmp/mx.png /tmp/mxr.png)" = 0 ]'
fi

pkill clipt; echo "== x11 $LAYOUT: $pass passed, $fail failed"
[ $fail -eq 0 ]
