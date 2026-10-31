#!/usr/bin/env python3
"""Drive GNOME Shell input through org.gnome.Mutter.RemoteDesktop (mutter has
no virtual-input Wayland protocols). Reads the same script as vinput from
stdin: move X Y, down, up, click, key NAME, ctrl NAME, type TEXT, sleep MS.
Absolute motion needs a screencast stream, so moves are relative from a
pointer first parked at (0, 0)."""
import sys, time
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION)
def call(path, iface, method, args=None, sig=None):
    return bus.call_sync("org.gnome.Mutter.RemoteDesktop", path, iface, method,
                         GLib.Variant(sig, args) if sig else None, None, 0, -1, None)

session = call("/org/gnome/Mutter/RemoteDesktop", "org.gnome.Mutter.RemoteDesktop", "CreateSession").unpack()[0]
S = "org.gnome.Mutter.RemoteDesktop.Session"
call(session, S, "Start")
KEYS = {"Escape": 1, "Return": 28, "BackSpace": 14, "ctrl": 29, "shift": 42}
for row, base in (("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44), ("1234567890", 2)):
    for i, c in enumerate(row):
        KEYS[c] = base + i
KEYS[" "] = 57

def key(code, down):
    call(session, S, "NotifyKeyboardKeycode", (code, down), "(ub)")
def tap(name, ctrl=False):
    shift = len(name) == 1 and name.isupper()
    code = KEYS[name.lower() if len(name) == 1 else name]
    if ctrl: key(KEYS["ctrl"], True)
    if shift: key(KEYS["shift"], True)
    key(code, True); key(code, False)
    if shift: key(KEYS["shift"], False)
    if ctrl: key(KEYS["ctrl"], False)
def button(down):
    call(session, S, "NotifyPointerButton", (0x110, down), "(ib)")

pos = [0.0, 0.0]
call(session, S, "NotifyPointerMotionRelative", (-10000.0, -10000.0), "(dd)")
time.sleep(0.3)
for line in sys.stdin:
    parts = line.split()
    if not parts:
        continue
    cmd, rest = parts[0], parts[1:]
    if cmd == "move":
        x, y = float(rest[0]), float(rest[1])
        call(session, S, "NotifyPointerMotionRelative", (x - pos[0], y - pos[1]), "(dd)")
        pos = [x, y]
    elif cmd == "down": button(True)
    elif cmd == "up": button(False)
    elif cmd == "click": button(True); button(False)
    elif cmd in ("key", "ctrl"): tap(rest[0], cmd == "ctrl")
    elif cmd == "type":
        for c in " ".join(rest): tap(c)
    elif cmd == "sleep": time.sleep(int(rest[0]) / 1000)
    time.sleep(0.02)
time.sleep(0.2)
call(session, S, "Stop")
