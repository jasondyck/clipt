#!/bin/bash
# Containerised end-to-end suites.
#   tests/e2e.sh                       build everything, run every suite
#   tests/e2e.sh [--no-build] IMAGE LAYOUT
#     IMAGE: sway | gnome   LAYOUT: single frac dual triple (sway), single dual (gnome)
set -e
cd "$(dirname "$0")/.."
build=1
[ "${1:-}" = --no-build ] && build=0 && shift
if [ $build = 1 ]; then
  cargo build --release --bin glint --example vinput
  docker build -q -t glint-sway tests/sway >/dev/null
  docker build -q -t glint-gnome tests/gnome >/dev/null
fi
B=$PWD/target/release
run() {
  case $1 in
    sway) docker run --rm --shm-size=512m -v $B/glint:/glint:ro -v $B/examples/vinput:/vinput:ro \
            -v $PWD/tests/sway/run.sh:/run.sh:ro glint-sway /run.sh $2 ;;
    gnome) docker run --rm --shm-size=512m -v $B/glint:/glint:ro -v $PWD/data/glint.desktop:/glint.desktop:ro \
            -v $PWD/tests/gnome:/t:ro -v $PWD/tests/gnome/rdinput.py:/rdinput.py:ro glint-gnome /t/gstart.sh /t/run.sh $2 ;;
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
exit $rc
