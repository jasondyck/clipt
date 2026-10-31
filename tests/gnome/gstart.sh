#!/bin/bash
# system bus + mocked logind, then a session bus running the test script
dbus-uuidgen --ensure; mkdir -p /run/dbus; dbus-daemon --system --fork
python3 -m dbusmock --system --template logind >/tmp/mock.log 2>&1 &
sleep 1
exec dbus-run-session "$@"
