#!/usr/bin/env python3
"""Put the second virtual monitor at a different scale, to the right of the
first (org.gnome.Mutter.DisplayConfig). Usage: set_scale.py SCALE"""
import sys
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION)
def call(method, args=None):
    return bus.call_sync("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                         "org.gnome.Mutter.DisplayConfig", method, args, None, 0, -1, None).unpack()

want = float(sys.argv[1])
serial, monitors, logical, props = call("GetCurrentState")
layout_mode = props.get("layout-mode", 1)  # 1 = logical, 2 = physical
cur = []
for (conn, *_), modes, _p in monitors:
    m = next(m for m in modes if m[6].get("is-current"))
    cur.append((conn, m))
(c0, m0), (c1, m1) = cur[0], cur[1]
scale1 = min(m1[5], key=lambda s: abs(s - want))  # nearest supported scale
x1 = m0[1] if layout_mode == 2 else int(m0[1] / 1.0)
cfg = [
    (0, 0, 1.0, 0, True, [(c0, m0[0], {})]),
    (x1, 0, scale1, 0, False, [(c1, m1[0], {})]),
]
call("ApplyMonitorsConfig", GLib.Variant("(uua(iiduba(ssa{sv}))a{sv})", (serial, 1, cfg, {})))
print(f"layout-mode={'physical' if layout_mode == 2 else 'logical'} {c0} {m0[1]}x{m0[2]}@1 + {c1} {m1[1]}x{m1[2]}@{scale1} supported={list(m1[5])}")
