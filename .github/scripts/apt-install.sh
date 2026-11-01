#!/bin/sh
# apt-get install with hard time limits, retried.
#   .github/scripts/apt-install.sh libxkbcommon-dev [more packages]
# A stalled Ubuntu mirror once left apt waiting silently for 20+ minutes on a
# download; apt's own Acquire timeouts didn't fire, so each attempt is killed
# from outside instead.
set -u
opts="-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30"
for attempt in 1 2 3; do
  # shellcheck disable=SC2086
  timeout 180 sudo apt-get $opts update &&
    timeout 300 sudo apt-get $opts install -y "$@" &&
    exit 0
  echo "apt-install: attempt $attempt failed, retrying" >&2
  sleep 10
done
echo "apt-install: giving up after 3 attempts" >&2
exit 1
