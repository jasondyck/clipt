#!/bin/bash
# Containerised end-to-end suites.
#   tests/e2e.sh                       build everything, run every suite
#   tests/e2e.sh [--no-build] IMAGE LAYOUT
#     IMAGE: sway | gnome | x11 | compositors
#     LAYOUT: single frac dual triple (sway), single dual (gnome), single multi (x11),
#             labwc niri (compositors)
set -e
cd "$(dirname "$0")/.."
build=1
[ "${1:-}" = --no-build ] && build=0 && shift
if [ $build = 1 ]; then
  cargo build --release --bin clipt --example vinput
  docker build -q -t clipt-sway tests/sway >/dev/null
  docker build -q -t clipt-gnome tests/gnome >/dev/null
  docker build -q -t clipt-x11 tests/x11 >/dev/null
  docker build -q -t clipt-compositors tests/compositors >/dev/null
fi
B=$PWD/target/release
run() {
  case $1 in
    sway) docker run --rm --shm-size=512m -v $B/clipt:/clipt:ro -v $B/examples/vinput:/vinput:ro \
            -v $PWD/tests/sway/run.sh:/run.sh:ro clipt-sway /run.sh $2 ;;
    gnome) docker run --rm --shm-size=512m -v $B/clipt:/clipt:ro -v $PWD/data/clipt.desktop:/clipt.desktop:ro \
            -v $PWD/tests/gnome:/t:ro -v $PWD/tests/gnome/rdinput.py:/rdinput.py:ro clipt-gnome /t/gstart.sh /t/run.sh $2 ;;
    x11) docker run --rm --shm-size=512m -v $B/clipt:/clipt:ro -v $PWD/tests/x11/run.sh:/run.sh:ro \
            -v $PWD/tests/x11/xorg-dummy.conf:/etc/X11/xorg-dummy.conf:ro clipt-x11 /run.sh $2 ;;
    compositors) docker run --rm --shm-size=512m -v $B/clipt:/clipt:ro -v $B/examples/vinput:/vinput:ro \
            -v $PWD/tests/compositors/run.sh:/run.sh:ro clipt-compositors /run.sh $2 ;;
  esac 2>&1 | grep -E "^(==|  (PASS|FAIL)|  stderr)"
  return ${PIPESTATUS[0]}
}
if [ $# -eq 2 ]; then
  run "$1" "$2"
  exit
fi
rc=0
for l in single frac dual triple; do run sway $l || rc=1; done
for l in single dual; do run gnome $l || rc=1; done
for l in single multi; do run x11 $l || rc=1; done
for c in labwc niri; do run compositors $c || rc=1; done
exit $rc
