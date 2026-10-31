#!/bin/bash
# What the sandbox takes away: runs native clipt and the installed Flatpak
# against the same compositor and prints what each could do.
#   tests/flatpak/probe.sh [sway labwc niri kwin]
# sway/labwc/niri run headless in the e2e containers (tests/e2e.sh builds the
# images); their Wayland socket is shared with the host, where `flatpak run`
# needs to be. kwin runs on the host (kwin_wayland --virtual) on a private
# session bus. Needs: packaging/flatpak/build.sh, cargo build --release.
# Everything runs on a private session bus so neither clipt can reach the real
# desktop's KWin or portal. The portal backend isn't probed: it's the fallback
# that works from a sandbox by design.
set -u
cd "$(dirname "$0")/../.."
APP=io.github.jasondyck.clipt
NATIVE=$PWD/target/release/clipt
comps=${*:-sway labwc niri kwin}

native()  { timeout 15 "$NATIVE" "$@"; }
flatpak_() { timeout 15 flatpak run "$APP" "$@"; }
# Flatpak with an unfiltered connection opened outside the sandbox.
launched_() { timeout 15 "$LAUNCH" flatpak run "$APP" "$@"; }
LAUNCH=$PWD/packaging/flatpak/clipt-launch

# Report one clipt (native, flatpak_ or launched_) against the current WAYLAND_DISPLAY.
report() {
  local who=$1 run=$2 out err
  err=$(mktemp)
  out=$($run list 2>"$err")
  if [ -z "$out" ]; then echo "  $who: list failed: $(tail -1 "$err")"; return; fi
  local backends=$(sed -n 's/^backends: //p' <<<"$out" | tr -d , | sed 's/ *portal//')
  echo "  $who: outputs $(grep -vc '^backends' <<<"$out"), backends [$backends]"
  for b in $backends; do
    if $run full -b "$b" -o - 2>"$err" | head -c 8 | grep -q PNG; then
      echo "    full -b $b: ok"
    else
      echo "    full -b $b: FAIL $(grep -v '^$' "$err" | tail -1)"
    fi
  done
  # The interesting globals: capture, overlay and window protocols.
  CLIPT_DEBUG=1 $run list 2>&1 >/dev/null | sed -n 's/^clipt: wayland globals: //p' | tr ' ' '\n' \
    | grep -E 'layer_shell|screencopy|image_copy|image_capture_source|foreign_toplevel|xdg_wm_base|security_context' \
    | tr '\n' ' ' | sed 's/^/    globals: /; s/$/\n/'
  local cmd=("$NATIVE")
  [ "$who" = flatpak ] && cmd=(flatpak run "$APP")
  [ "$who" = launched ] && cmd=("$LAUNCH" flatpak run "$APP")
  CLIPT_DEBUG=1 timeout -k 2 -s INT 4 "${cmd[@]}" gui >/dev/null 2>"$err"
  echo "    overlay: $(sed -n 's/^clipt: overlay via //p' "$err" | head -1)"
  echo "    windows: $(grep -E 'windows in|window list unavailable' "$err" | head -1 | sed 's/^clipt: //')"
  rm -f "$err"
}

wait_sock() { for _ in $(seq 60); do ls "$1"/wayland-? >/dev/null 2>&1 && return; sleep 0.25; done; return 1; }

for c in $comps; do
  echo "== $c"
  rt=$(mktemp -d)
  cid=
  case $c in
    sway|labwc|niri)
      img=clipt-compositors; [ "$c" = sway ] && img=clipt-sway
      case $c in
        sway)  start='sway -c /dev/null' ;;
        labwc) start='labwc' ;;
        niri)  start='printf "output * mode 1280x720\n" >/tmp/s.conf; sway -c /tmp/s.conf & sleep 2; mv $XDG_RUNTIME_DIR/wayland-1 $XDG_RUNTIME_DIR/outer; mv $XDG_RUNTIME_DIR/wayland-1.lock $XDG_RUNTIME_DIR/outer.lock; printf "hotkey-overlay {\n skip-at-startup\n}\n" >/tmp/n.kdl; WAYLAND_DISPLAY=outer niri -c /tmp/n.kdl' ;;
      esac
      cid=$(docker run -d --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e WLR_BACKENDS=headless \
              -v "$rt:/tmp/xdg" "$img" sh -c "$start >/tmp/comp.log 2>&1 & sleep infinity")
      if [ "$c" = niri ]; then
        # Sway's socket comes first and is renamed to outer; then niri's appears.
        for _ in $(seq 60); do [ -e "$rt/outer" ] && break; sleep 0.25; done
      fi
      wait_sock "$rt" || { echo "  compositor didn't start"; docker rm -f "$cid" >/dev/null; continue; }
      sleep 1
      sock=$(ls "$rt"/wayland-? | head -1)
      export XDG_CURRENT_DESKTOP=$c
      [ "$c" = sway ] && export SWAYSOCK=$(ls "$rt"/sway-ipc.* 2>/dev/null | head -1)
      WAYLAND_DISPLAY=$sock dbus-run-session -- bash -c "$(declare -f report native flatpak_ launched_); APP=$APP NATIVE=$NATIVE LAUNCH=$LAUNCH
        report native native; report flatpak flatpak_; report launched launched_" 2>/dev/null
      unset SWAYSOCK
      docker rm -f "$cid" >/dev/null ;;
    kwin)
      # A private bus, so this KWin's ScreenShot2 doesn't meet a real session's.
      dbus-run-session -- bash -c '
        export XDG_CURRENT_DESKTOP=KDE KDE_FULL_SESSION=true
        kwin_wayland --virtual --width 1280 --height 720 --socket clipt-probe >/tmp/kwin-probe.log 2>&1 &
        for _ in $(seq 60); do [ -S "$XDG_RUNTIME_DIR/clipt-probe" ] && break; sleep 0.25; done
        sleep 2
        export WAYLAND_DISPLAY=clipt-probe
        '"$(declare -f report native flatpak_); APP=$APP NATIVE=$NATIVE"'
        report native native
        report flatpak flatpak_
        kill %1' 2>/dev/null ;;
  esac
  rm -rf "$rt"
done
