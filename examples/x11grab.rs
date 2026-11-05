//! Test popup for the X11 end-to-end tests: an override-redirect window in
//! one colour that grabs the pointer and keyboard, as an open `<select>` or
//! menu does, and exits when Escape is pressed. Prints `grabbed` once it holds
//! both grabs.
//!
//!   x11grab [--ignore-escape] [--pointer-only] [RRGGBB X Y W H]
//!
//! (default 20c0c0 600 300 200 150). With --ignore-escape it keeps its grab
//! until killed. With --pointer-only it grabs just the pointer and takes the
//! input focus instead of the keyboard, as some menus do.

use std::io::Write;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, EventMask, GrabMode, GrabStatus, InputFocus, WindowClass};
use x11rb::wrapper::ConnectionExt as _;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |name: &str| {
        let i = args.iter().position(|a| a == name);
        i.map(|i| args.remove(i)).is_some()
    };
    let ignore = flag("--ignore-escape");
    let pointer_only = flag("--pointer-only");
    let num = |i: usize, d: i32| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
    let colour = args.first().and_then(|c| u32::from_str_radix(c, 16).ok()).unwrap_or(0x20c0c0);
    let (x, y, w, h) = (num(1, 600), num(2, 300), num(3, 200), num(4, 150));

    let (conn, n) = x11rb::connect(None).expect("X11");
    let screen = &conn.setup().roots[n];
    let win = conn.generate_id().unwrap();
    let aux = CreateWindowAux::new().override_redirect(1).background_pixel(colour).event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS);
    conn.create_window(0, win, screen.root, x as i16, y as i16, w as u16, h as u16, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
    conn.map_window(win).unwrap();
    conn.sync().unwrap();

    let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
    let (mut ptr, mut kb) = (false, pointer_only);
    for _ in 0..200 {
        if !ptr {
            ptr = conn
                .grab_pointer(true, win, mask, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME)
                .unwrap()
                .reply()
                .unwrap()
                .status
                == GrabStatus::SUCCESS;
        }
        if !kb {
            kb = conn.grab_keyboard(true, win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).unwrap().reply().unwrap().status
                == GrabStatus::SUCCESS;
        }
        if ptr && kb {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(ptr && kb, "couldn't grab");
    if pointer_only {
        conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME).unwrap();
        conn.sync().unwrap();
    }
    println!("grabbed");
    std::io::stdout().flush().unwrap();

    let map = conn.get_keyboard_mapping(conn.setup().min_keycode, conn.setup().max_keycode - conn.setup().min_keycode + 1);
    let map = map.unwrap().reply().unwrap();
    let per = map.keysyms_per_keycode as usize;
    loop {
        if let Event::KeyPress(e) = conn.wait_for_event().unwrap() {
            let sym = map.keysyms.get((e.detail - conn.setup().min_keycode) as usize * per).copied();
            if sym == Some(0xff1b) && !ignore {
                return;
            }
        }
    }
}
