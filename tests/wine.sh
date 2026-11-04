#!/bin/bash
# The Windows build under Wine: cross-builds clipt.exe (MinGW, in Docker),
# then runs tests/wine/run.sh in a container. Needs Docker and
# `rustup target add x86_64-pc-windows-gnu`. With CLIPT_FFMPEG_EXE set to a
# Windows ffmpeg.exe (e.g. from github.com/BtbN/FFmpeg-Builds), it also
# records.
set -e
cd "$(dirname "$0")/.."
docker build -q -t clipt-x11 tests/x11 >/dev/null
docker build -q -t clipt-wine tests/wine >/dev/null
PATH=$PWD/tests/wine/bin:$PATH CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release --target x86_64-pc-windows-gnu
ffmpeg=()
[ -n "${CLIPT_FFMPEG_EXE:-}" ] && ffmpeg=(-v "$(realpath "$CLIPT_FFMPEG_EXE"):/ffmpeg.exe:ro")
docker run --rm --shm-size=512m -v $PWD/target/x86_64-pc-windows-gnu/release/clipt.exe:/clipt.exe:ro "${ffmpeg[@]}" \
  -v $PWD/tests/wine/run.sh:/run.sh:ro -v $PWD/tests/audio.sh:/audio.sh:ro clipt-wine /run.sh 2>&1 | grep -E "^(==|  (PASS|FAIL))"
exit ${PIPESTATUS[0]}
