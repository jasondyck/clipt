//! Test driver: a long-lived virtual pointer + keyboard (wlroots protocols)
//! that plays a script from stdin, for end-to-end tests under headless Sway.
//!
//!   vinput KEYMAP_FILE LAYOUT_W LAYOUT_H < script
//!
//! Script lines: `move X Y`, `down`, `up`, `click`, `rdown`, `rup`,
//! `key NAME`, `ctrl NAME`, `type TEXT`, `scroll N`, `sleep MS`.
//! Keys are evdev names below; letters/digits map directly.

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_pointer, wl_registry, wl_seat},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct S;

fn keycode(name: &str) -> Option<(u32, bool)> {
    // (evdev code, needs shift)
    let row = |s: &str, base: u32| s.find(name).map(|i| (base + i as u32, false));
    if name.len() == 1 {
        let c = name.chars().next().unwrap();
        if c.is_ascii_uppercase() {
            return keycode(&c.to_ascii_lowercase().to_string()).map(|(k, _)| (k, true));
        }
        return row("qwertyuiop", 16).or_else(|| row("asdfghjkl", 30)).or_else(|| row("zxcvbnm", 44)).or_else(|| row("1234567890", 2)).or(
            match c {
                ' ' => Some((57, false)),
                '[' => Some((26, false)),
                ']' => Some((27, false)),
                _ => None,
            },
        );
    }
    Some((
        match name {
            "Escape" => 1,
            "Return" => 28,
            "BackSpace" => 14,
            "Left" => 105,
            "Right" => 106,
            "Up" => 103,
            "Down" => 108,
            _ => return None,
        },
        false,
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let keymap = std::fs::read(&args[1]).expect("keymap file");
    let (lw, lh): (u32, u32) = (args[2].parse().unwrap(), args[3].parse().unwrap());

    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut q) = registry_queue_init::<S>(&conn).unwrap();
    let qh = q.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).unwrap();
    let pm: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let km: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let ptr = pm.create_virtual_pointer(Some(&seat), &qh, ());
    let kbd = km.create_virtual_keyboard(&seat, &qh, ());

    let fd = rustix::fs::memfd_create("km", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut f = std::fs::File::from(fd);
    f.write_all(&keymap).unwrap();
    f.write_all(&[0]).unwrap();
    kbd.keymap(1, f.as_fd(), keymap.len() as u32 + 1);
    q.roundtrip(&mut S).unwrap();

    let t0 = std::time::Instant::now();
    let now = || t0.elapsed().as_millis() as u32;
    let flush = |q: &mut wayland_client::EventQueue<S>| {
        q.roundtrip(&mut S).unwrap();
    };
    let press = |code: u32, shift: bool, ctrl: bool| {
        let mods = (shift as u32) | ((ctrl as u32) << 2);
        kbd.modifiers(mods, 0, 0, 0);
        kbd.key(now(), code, 1);
        kbd.key(now(), code, 0);
        kbd.modifiers(0, 0, 0, 0);
    };
    let button = |b: u32, down: bool| {
        let st = if down { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        ptr.button(now(), b, st);
        ptr.frame();
    };

    // Give clients a moment to notice the new seat capabilities.
    std::thread::sleep(std::time::Duration::from_millis(300));
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let mut it = line.split_whitespace();
        let Some(cmd) = it.next() else { continue };
        let rest: Vec<&str> = it.collect();
        match cmd {
            "move" => {
                let (x, y): (u32, u32) = (rest[0].parse().unwrap(), rest[1].parse().unwrap());
                ptr.motion_absolute(now(), x, y, lw, lh);
                ptr.frame();
            }
            "down" => button(0x110, true),
            "up" => button(0x110, false),
            "click" => {
                button(0x110, true);
                button(0x110, false);
            }
            "rdown" => button(0x111, true),
            "rup" => button(0x111, false),
            "key" | "ctrl" => {
                let (k, shift) = keycode(rest[0]).unwrap_or_else(|| panic!("unknown key {}", rest[0]));
                press(k, shift, cmd == "ctrl");
            }
            "type" => {
                for c in rest.join(" ").chars() {
                    let (k, shift) = keycode(&c.to_string()).unwrap();
                    press(k, shift, false);
                }
            }
            "scroll" => {
                let n: f64 = rest[0].parse().unwrap();
                ptr.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);
                ptr.axis_discrete(now(), wl_pointer::Axis::VerticalScroll, n * 15.0, n as i32);
                ptr.frame();
            }
            "sleep" => {
                flush(&mut q);
                std::thread::sleep(std::time::Duration::from_millis(rest[0].parse().unwrap()));
            }
            _ => panic!("bad command {line}"),
        }
        flush(&mut q);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(S: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(S: ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(S: ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(S: ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(S: ZwpVirtualKeyboardV1);
