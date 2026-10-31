#!/bin/bash
# Run the containerised end-to-end suites. Builds images on first use.
set -e
cd "$(dirname "$0")/.."
cargo build --release --bin glint --example vinput
docker build -q -t glint-sway tests/sway >/dev/null
docker build -q -t glint-gnome tests/gnome >/dev/null
B=$PWD/target/release
rc=0
for l in single frac dual triple; do
  docker run --rm --shm-size=512m -v $B/glint:/glint:ro -v $B/examples/vinput:/vinput:ro -v $PWD/tests/sway/run.sh:/run.sh:ro \
    glint-sway /run.sh $l 2>&1 | grep -E "^(==|  (PASS|FAIL))" || rc=1
done
for l in single dual; do
  docker run --rm --shm-size=512m -v $B/glint:/glint:ro -v $PWD/data/glint.desktop:/glint.desktop:ro -v $PWD/tests/gnome:/t:ro \
    -v $PWD/tests/gnome/rdinput.py:/rdinput.py:ro glint-gnome /t/gstart.sh /t/run.sh $l 2>&1 | grep -E "^(==|  (PASS|FAIL))" || rc=1
done
exit $rc
