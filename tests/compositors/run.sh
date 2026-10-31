#!/bin/bash
# Generic Wayland compositor suite. Usage: run.sh labwc|niri
set -u
COMP=${1:-labwc}
mkdir -p $XDG_RUNTIME_DIR && chmod 700 $XDG_RUNTIME_DIR
export HOME=/tmp/home && mkdir -p $HOME
pass=0 fail=0
ok()  { echo "  PASS $*"; pass=$((pass+1)); }
bad() { echo "  FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1  [$2]"; fi; }
size() { magick identify -format '%wx%h' "$1" 2>/dev/null; }
px()   { magick "$1" -format "%[pixel:p{$2}]" info: 2>/dev/null; }
ae()   { magick compare -metric AE "$1" "$2" null: 2>&1 | cut -d" " -f1; }
pastesize() { wl-paste -t image/png 2>/dev/null | magick identify -format %wx%h - 2>/dev/null; }
sockets() { ls $XDG_RUNTIME_DIR | grep -E "^wayland-[0-9]+$" | sort; }

magick -size 1920x1080 gradient:'#d02020-#2040e0' -fill white -draw 'rectangle 600,300 1300,700' /tmp/bg.png
case $COMP in
  labwc)
    WLR_BACKENDS=headless labwc >/tmp/comp.log 2>&1 &
    for _ in $(seq 40); do [ -n "$(sockets)" ] && break; sleep 0.25; done
    export WAYLAND_DISPLAY=$(sockets | head -1) ;;
  niri)
    # niri runs as a window inside a headless sway (its winit backend).
    printf 'output * mode 1280x720\ndefault_border none\n' > /tmp/sway.conf
    WLR_BACKENDS=headless sway -c /tmp/sway.conf >/tmp/sway.log 2>&1 &
    for _ in $(seq 40); do [ -n "$(sockets)" ] && break; sleep 0.25; done
    outer=$(sockets | head -1)
    # niri pops up its hotkey help on first start; keep it off the captures.
    printf 'hotkey-overlay {\n    skip-at-startup\n}\n' > /tmp/niri.kdl
    WAYLAND_DISPLAY=$outer niri -c /tmp/niri.kdl >/tmp/comp.log 2>&1 &
    for _ in $(seq 60); do [ "$(sockets | wc -l)" -ge 2 ] && break; sleep 0.25; done
    export WAYLAND_DISPLAY=$(sockets | grep -vx "$outer" | head -1) ;;
esac
swaybg -i /tmp/bg.png -m fill >/dev/null 2>&1 &
sleep 2
echo "== $COMP"
/clipt list | sed 's/^/  /'
read LW LH < <(/clipt list | grep -v backends | awk '{split($2,a,/[x+]/); r=a[3]+a[1]; b=a[4]+a[2]; if (r>R) R=r; if (b>B) B=b} END {print R, B}')
OUT=$(/clipt list | head -1 | cut -f1)

echo "-- headless capture vs grim"
for b in $(/clipt list | sed -n 's/^backends: //p' | tr -d ','); do
  /clipt full -b $b -o /tmp/f-$b.png; grim /tmp/grim.png
  check "full -b $b $(size /tmp/f-$b.png) matches grim (AE=$(ae /tmp/f-$b.png /tmp/grim.png))" '[ "$(ae /tmp/f-$b.png /tmp/grim.png)" = 0 ]'
done
/clipt full -g "100,50 300x200" -o /tmp/g.png; grim -g "100,50 300x200" /tmp/gg.png
check "geometry crop matches grim (AE=$(ae /tmp/g.png /tmp/gg.png))" '[ "$(ae /tmp/g.png /tmp/gg.png)" = 0 ]'

echo "-- headless clipboard"
/clipt full -c; sleep 0.5
wl-paste -t image/png > /tmp/p.png 2>/dev/null
check "wl-paste returns the capture" '[ "$(ae /tmp/p.png /tmp/grim.png)" = 0 ]'
echo hi | wl-copy; sleep 0.5
check "daemon exits once replaced" '! pgrep -f "^/clipt full -c" >/dev/null'

echo "-- interactive (layer-shell overlay, virtual pointer/keyboard)"
xkbcli compile-keymap --layout us > /tmp/km 2>/dev/null || cp /usr/share/X11/xkb/compiled/us /tmp/km 2>/dev/null
mkfifo /tmp/vin; /vinput /tmp/km $LW $LH < /tmp/vin 2>/tmp/vinput.err & exec 3>/tmp/vin
act() { for a in "$@"; do echo "$a" >&3; done; }
drag() { act "move $1 $2" down "move $(( ($1+$3)/2 )) $(( ($2+$4)/2 ))" "move $3 $4" up; sleep 0.4; }
gui() { /clipt gui 2>/tmp/gui.err & sleep 1; }
sleep 0.5
gui; drag 100 100 400 300; act "key Return"; sleep 0.8
wl-paste -t image/png > /tmp/sel.png 2>/dev/null; grim -g "100,100 300x200" /tmp/selg.png
check "drag + Enter copies 300x200 (got $(size /tmp/sel.png), AE=$(ae /tmp/sel.png /tmp/selg.png))" '[ "$(ae /tmp/sel.png /tmp/selg.png)" = 0 ]'
gui; drag 50 50 450 350; act "key r"; drag 100 100 300 250; act "ctrl s"; sleep 0.8
f=$(ls $HOME/Pictures/Screenshots/*.png 2>/dev/null | head -1)
check "Ctrl+S saved 400x300 (got $(size "$f"))" '[ "$(size "$f")" = 400x300 ]'
check "red rectangle stroke ($(px "$f" 50,125))" 'px "$f" 50,125 | grep -Eq "srgba?\((2[0-9][0-9]|1[6-9][0-9]),[0-9]{1,2},[0-9]{1,2}"'
echo marker | wl-copy; sleep 0.3
gui; drag 10 10 200 200; act "key Escape"; sleep 0.6
check "Esc exits without touching the clipboard" '! pgrep -f "^/clipt gui" >/dev/null && [ "$(wl-paste -n 2>/dev/null)" = marker ]'
gui; act "move 300 200" click; sleep 0.3; act "key Return"; sleep 0.8
check "click copies the whole output (${LW}x${LH}, got $(pastesize))" '[ "$(pastesize)" = "${LW}x${LH}" ]'
grep -v "notification failed" /tmp/gui.err | sed 's/^/  stderr: /' | head -3
exec 3>&-
pkill clipt; echo "== $COMP: $pass passed, $fail failed"
[ $fail -eq 0 ]
