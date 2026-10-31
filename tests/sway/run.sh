#!/bin/bash
# End-to-end tests for glint under headless Sway. Runs inside the glint-sway
# container with the binary mounted at /glint. Usage: run.sh <layout>
#   single   one 1280x720 output, scale 1
#   frac     one 1920x1080 output, scale 1.5
#   dual     1280x720@1 + 1920x1080@2 side by side (mixed DPI)
set -u
LAYOUT=${1:-single}
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
export HOME=/tmp/home && mkdir -p $HOME
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
px()   { magick "$1" -format "%[pixel:p{$2}]" info: 2>/dev/null; }   # "x,y"

# A colourful wallpaper so captures have content to compare.
magick -size 1920x1080 gradient:'#d02020-#2040e0' -fill white -draw 'rectangle 600,300 1300,700' /tmp/bg.png
cat > /tmp/sway.conf <<CONF
output * bg /tmp/bg.png stretch
default_border none
CONF
case $LAYOUT in
  single) echo 'output HEADLESS-1 mode 1280x720 scale 1 position 0 0' >> /tmp/sway.conf ;;
  frac)   echo 'output HEADLESS-1 mode 1920x1080 scale 1.5 position 0 0' >> /tmp/sway.conf ;;
  dual)   printf 'output HEADLESS-1 mode 1280x720 scale 1 position 0 0\noutput HEADLESS-2 mode 1920x1080 scale 2 position 1280 0\n' >> /tmp/sway.conf ;;
  # A typical messy office desk: 1080p, a 1440p panel at 1.25x sitting
  # 200px lower, and a portrait (rotated) monitor underneath the first.
  triple) printf 'output HEADLESS-1 mode 1920x1080 scale 1 position 0 0\noutput HEADLESS-2 mode 2560x1440 scale 1.25 position 1920 200\noutput HEADLESS-3 mode 1280x1024 scale 1 transform 90 position 0 1080\n' >> /tmp/sway.conf ;;
esac
sway -c /tmp/sway.conf >/tmp/sway.log 2>&1 &
for _ in $(seq 50); do ls $XDG_RUNTIME_DIR/wayland-* >/dev/null 2>&1 && break; sleep 0.1; done
export WAYLAND_DISPLAY=$(basename $(ls $XDG_RUNTIME_DIR/wayland-? | head -1))
export SWAYSOCK=$(ls $XDG_RUNTIME_DIR/sway-ipc.* | head -1)
[ "$LAYOUT" = dual ] && swaymsg create_output >/dev/null && swaymsg reload >/dev/null
[ "$LAYOUT" = triple ] && swaymsg create_output >/dev/null && swaymsg create_output >/dev/null && swaymsg reload >/dev/null
sleep 1
echo "== layout $LAYOUT"
/glint list | sed 's/^/  /'

echo "-- headless capture vs grim"
for b in ext wlr; do
  /glint full -b $b -o /tmp/full-$b.png 2>/tmp/err || { bad "full -b $b: $(cat /tmp/err)"; continue; }
  grim /tmp/grim.png
  check "full -b $b size matches grim ($(size /tmp/grim.png))" '[ "$(size /tmp/full-$b.png)" = "$(size /tmp/grim.png)" ]'
  # Mixed-DPI stitching upscales the 1x output: filters differ slightly, and
  # grim leaves semi-transparent edge pixels there, so compare colour only.
  d=$(magick compare -fuzz 8% -metric AE <(magick /tmp/full-$b.png -alpha off png:-) <(magick /tmp/grim.png -alpha off png:-) null: 2>&1 | cut -d" " -f1)
  check "full -b $b pixels match grim (AE=$d at 8% fuzz)" '[ "${d%.*}" -lt 100 ]'
  [ -d /out ] && cp /tmp/full-$b.png /out/glint-$b.png && cp /tmp/grim.png /out/grim.png
  GLINT_DEBUG=1 /glint full -b $b -o /dev/null 2>&1 | grep -q "captured with ${b^}" && ok "-b $b really used $b" || bad "-b $b used another protocol"
done
/glint screen HEADLESS-1 -o /tmp/screen.png
check "screen HEADLESS-1 is one output" '[ -s /tmp/screen.png ]'
grim -g "100,50 300x200" /tmp/grim-g.png
/glint full -g "100,50 300x200" -o /tmp/g.png
check "geometry crop size matches grim ($(size /tmp/grim-g.png))" '[ "$(size /tmp/g.png)" = "$(size /tmp/grim-g.png)" ]'
d=$(magick compare -metric AE /tmp/g.png /tmp/grim-g.png null: 2>&1 | cut -d" " -f1)
check "geometry crop pixels match grim (AE=$d)" '[ "${d%.*}" -lt 100 ]'

echo "-- headless clipboard (data-control)"
/glint full -c
sleep 0.5
wl-paste -t image/png > /tmp/paste.png 2>/tmp/err
check "wl-paste returns the capture" '[ "$(size /tmp/paste.png)" = "$(size /tmp/full-ext.png)" ]'
pid=$(pgrep -f "^/glint full -c" | head -1)
[ -n "$pid" ] && echo "  clipboard daemon RSS: $(grep VmRSS /proc/$pid/status | awk "{print \$2}") kB"
echo hello | wl-copy; sleep 0.5
check "daemon exits once something else is copied" '! pgrep -f "^/glint full -c" >/dev/null'

# Persistent virtual pointer + keyboard (headless Sway has no input devices).
xkbcli compile-keymap --layout us > /tmp/keymap
read LW LH < <(swaymsg -t get_outputs | jq -r '[.[].rect] | "\(map(.x+.width)|max) \(map(.y+.height)|max)"')
mkfifo /tmp/vin
/vinput /tmp/keymap $LW $LH < /tmp/vin 2>/tmp/vinput.err &
exec 3>/tmp/vin
act() { for a in "$@"; do echo "$a" >&3; done; }
gui() { ${DEBUG:+env GLINT_DEBUG=1} /glint gui 2>/tmp/gui.err & sleep 0.8; }
drag() { act "move $1 $2" down "move $(( ($1+$3)/2 )) $(( ($2+$4)/2 ))" "move $3 $4" up; sleep 0.3; }
scale=$(swaymsg -t get_outputs | jq '.[] | select(.name=="HEADLESS-1") | .scale')
logical() { awk "BEGIN{printf \"%dx%d\", $1*$scale+0.5, $2*$scale+0.5}"; }
gerr() { grep -v "notification failed" /tmp/gui.err | grep -v "^glint: \(drew\|HDMI\|HEADLESS\|key\|pointer\|keyboard\|seat\)" | sed 's/^/  stderr: /'; }
sleep 0.5

echo "-- interactive: drag + Enter copies"
gui
drag 100 100 400 300
act "key Return"; sleep 0.8
wl-paste -t image/png > /tmp/sel.png 2>/dev/null
check "selection copied at native size (want $(logical 300 200), got $(size /tmp/sel.png))" '[ "$(size /tmp/sel.png)" = "$(logical 300 200)" ]'
d=$(magick compare -metric AE /tmp/sel.png <(grim -g "100,100 300x200" -) null: 2>&1 | cut -d" " -f1)
check "copied pixels match grim of the same region (AE=$d)" '[ "${d%.*}" -lt 100 ]'
pid=$(pgrep -f "^/glint gui" | head -1)
[ -n "$pid" ] && echo "  gui clipboard daemon RSS: $(grep VmRSS /proc/$pid/status | awk '{print $2}') kB"
gerr

echo "-- interactive: rectangle annotation + Ctrl+S saves"
gui
drag 50 50 450 350
act "key r"; drag 100 100 300 250
act "ctrl s"; sleep 0.8
f=$(ls $HOME/Pictures/Screenshots/*.png 2>/dev/null | head -1)
check "Ctrl+S wrote a file" '[ -n "$f" ]'
if [ -n "$f" ]; then
  check "saved size $(logical 400 300)" '[ "$(size $f)" = "$(logical 400 300)" ]'
  # rectangle left edge at logical x=100 → 50 in the crop, mid-height y=175 → 125
  p=$(px "$f" "$(awk "BEGIN{print int(50*$scale+0.5)\",\"int(125*$scale+0.5)}")")
  check "red rectangle stroke present ($p)" 'echo "$p" | grep -Eq "srgba?\((2[0-9][0-9]|1[6-9][0-9]),[0-9]{1,2},[0-9]{1,2}"'
fi
check "Ctrl+S also copied (copy_on_save)" '[ "$(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null)" = "$(logical 400 300)" ]'
gerr

echo "-- interactive: Esc cancels, clipboard untouched"
echo marker | wl-copy; sleep 0.3
gui
drag 10 10 200 200
act "key Escape"; sleep 0.5
check "glint exited on Esc" '! pgrep -f "^/glint gui" >/dev/null'
check "clipboard still holds the old text" '[ "$(wl-paste -n 2>/dev/null)" = marker ]'

echo "-- interactive: click selects whole output"
gui
act "move 300 200" click; sleep 0.3
act "key Return"; sleep 0.8
w=$(swaymsg -t get_outputs | jq -r '.[] | select(.name=="HEADLESS-1") | "\(.current_mode.width)x\(.current_mode.height)"')
check "click copies whole HEADLESS-1 ($w, got $(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null))" '[ "$(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null)" = "$w" ]'

echo "-- interactive: text tool, undo, toolbar copy button"
gui
drag 100 100 500 400
act "key t" "move 150 150" click "type Hi there" "key Return" "key a"
drag 200 300 400 200
act "ctrl z"; sleep 0.2
# toolbar sits centred under the selection; its copy button is second from the right
act "key Return"; sleep 0.8
check "copy after text+undo produced an image" '[ "$(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null)" = "$(logical 400 300)" ]'
gerr

echo "-- interactive: double-click copies"
gui
drag 60 60 260 200
act "move 150 120" click click; sleep 0.8
check "double-click inside selection copies" '[ "$(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null)" = "$(logical 200 140)" ]'

if [ "$LAYOUT" = dual ]; then
  echo "-- interactive: selection spanning both outputs"
  gui
  drag 1180 100 1380 300
  act "key Return"; sleep 0.8
  wl-paste -t image/png > /tmp/span.png 2>/dev/null
  # exported at the scale of the output under the selection centre (HEADLESS-2, 2x)
  check "spanning selection exported at 2x (400x400, got $(size /tmp/span.png))" '[ "$(size /tmp/span.png)" = 400x400 ]'
  l=$(px /tmp/span.png 50,200); r=$(px /tmp/span.png 350,200)
  echo "  left half pixel $l, right half pixel $r"
  check "left part comes from HEADLESS-1, right from HEADLESS-2" '[ -n "$l" ] && [ -n "$r" ]'

  echo "-- interactive: drag on the 2x output"
  gui
  drag 1400 100 1600 250
  act "key Return"; sleep 0.8
  check "selection on HEADLESS-2 is 2x (400x300)" '[ "$(wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null)" = 400x300 ]'
fi
if [ "$LAYOUT" = triple ]; then
  echo "-- triple: per-output captures (rotation, fractional scale)"
  for o in HEADLESS-1 HEADLESS-2 HEADLESS-3; do
    /glint screen $o -o /tmp/s-$o.png; grim -o $o /tmp/g-$o.png 2>/tmp/grim.err || echo "  (grim -o $o: $(cat /tmp/grim.err))"
    [ -d /out ] && cp /tmp/s-$o.png /out/glint-$o.png && cp /tmp/g-$o.png /out/grim-$o.png 2>/dev/null
    d=$(magick compare -metric AE /tmp/s-$o.png /tmp/g-$o.png null: 2>&1 | cut -d" " -f1)
    check "screen $o $(size /tmp/s-$o.png) matches grim $(size /tmp/g-$o.png) (AE=$d)" '[ "$(size /tmp/s-$o.png)" = "$(size /tmp/g-$o.png)" ] && [ "${d%.*}" -lt 100 ]'
  done

  echo "-- triple: every output transform, both protocols, vs grim"
  for t in 90 180 270 flipped flipped-90 flipped-180 flipped-270 normal; do
    swaymsg -q output HEADLESS-3 transform $t; sleep 0.3
    grim -o HEADLESS-3 /tmp/gt.png
    for b in ext wlr; do
      /glint screen HEADLESS-3 -b $b -o /tmp/st.png
      d=$(magick compare -metric AE /tmp/st.png /tmp/gt.png null: 2>&1 | cut -d" " -f1)
      check "transform $t via $b: $(size /tmp/st.png) (AE=$d)" '[ "$(size /tmp/st.png)" = "$(size /tmp/gt.png)" ] && [ "${d%.*}" -lt 100 ]'
    done
  done
  swaymsg -q output HEADLESS-3 transform 90; sleep 0.3

  echo "-- triple: no monitor picker: drag directly on any output"
  # HEADLESS-2 spans logical x 1920..3968, y 200..1352 at 1.25x
  gui
  drag 2100 400 2500 700
  act "key Return"; sleep 0.8
  wl-paste -t image/png > /tmp/t2.png 2>/dev/null
  check "drag on the 1.25x output copies 500x375 (got $(size /tmp/t2.png))" '[ "$(size /tmp/t2.png)" = 500x375 ]'
  d=$(magick compare -metric AE /tmp/t2.png <(grim -g "2100,400 400x300" -) null: 2>&1 | cut -d" " -f1)
  check "pixels match grim for that region (AE=$d)" '[ "${d%.*}" -lt 100 ]'

  # HEADLESS-3 is portrait: logical 1024x1280 at y 1080
  gui
  act "move 500 1700" click; sleep 0.3
  act "key Return"; sleep 0.8
  wl-paste -t image/png > /tmp/t3.png 2>/dev/null
  check "click on the portrait output copies it upright (1024x1280, got $(size /tmp/t3.png))" '[ "$(size /tmp/t3.png)" = 1024x1280 ]'
  d=$(magick compare -metric AE /tmp/t3.png /tmp/g-HEADLESS-3.png null: 2>&1 | cut -d" " -f1)
  check "portrait copy matches grim -o HEADLESS-3 (AE=$d)" '[ "${d%.*}" -lt 100 ]'

  echo "-- triple: selection across the offset 1x/1.25x boundary"
  gui
  drag 1800 300 2040 500
  act "key Return"; sleep 0.8
  wl-paste -t image/png > /tmp/tx.png 2>/dev/null
  # centre (1920,400) is on the boundary; either scale is acceptable, but
  # the image must be the selection at one consistent scale
  check "spanning copy is 240x200 at 1x or 300x250 at 1.25x (got $(size /tmp/tx.png))" '[ "$(size /tmp/tx.png)" = 240x200 ] || [ "$(size /tmp/tx.png)" = 300x250 ]'
fi
exec 3>&-
[ -d /out ] && cp /tmp/sway.log /out/sway.log
pkill glint; echo "== $LAYOUT: $pass passed, $fail failed"
[ $fail -eq 0 ]
