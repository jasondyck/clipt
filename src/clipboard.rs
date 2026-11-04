//! Headless clipboard via data-control (ext-data-control-v1 or
//! wlr-data-control-unstable-v1): lets `clipt full -c` copy without showing a
//! window. KDE and wlroots compositors have one of these; GNOME has neither.
//! The interactive editor doesn't need this: it copies through its own focused
//! surface (see overlay.rs).

use anyhow::{Context, Result};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

#[derive(Default)]
struct State {
    data: Arc<Vec<u8>>,
    done: bool,
}

macro_rules! data_control {
    ($modname:ident, $mgr_ty:ty, $dev:ident, $src:ident, $mgr_mod:path, $dev_mod:ident, $src_mod:ident, $offer_ty:ty) => {
        mod $modname {
            use super::*;
            pub use $mgr_mod as m;
            use m::{$dev_mod, $src_mod};

            pub fn set(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<State>, seat: &wl_seat::WlSeat, mime: &str) -> Option<()> {
                let mgr: $mgr_ty = globals.bind(qh, 1..=1, ()).ok()?;
                let dev = mgr.get_data_device(seat, qh, ());
                let src = mgr.create_data_source(qh, ());
                src.offer(mime.to_string());
                dev.set_selection(Some(&src));
                Some(())
            }

            impl Dispatch<$src_mod::$src, ()> for State {
                fn event(s: &mut Self, _: &$src_mod::$src, ev: $src_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                    match ev {
                        $src_mod::Event::Send { fd, .. } => serve(s.data.clone(), fd),
                        $src_mod::Event::Cancelled => s.done = true,
                        _ => {}
                    }
                }
            }
            impl Dispatch<$dev_mod::$dev, ()> for State {
                fn event(_: &mut Self, _: &$dev_mod::$dev, _: $dev_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
                wayland_client::event_created_child!(State, $dev_mod::$dev, [
                    $dev_mod::EVT_DATA_OFFER_OPCODE => ($offer_ty, ()),
                ]);
            }
            wayland_client::delegate_noop!(State: ignore $offer_ty);
            wayland_client::delegate_noop!(State: $mgr_ty);
        }
    };
}

data_control!(
    ext,
    wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ExtDataControlSourceV1,
    wayland_protocols::ext::data_control::v1::client,
    ext_data_control_device_v1,
    ext_data_control_source_v1,
    wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1
);

data_control!(
    wlr,
    wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    ZwlrDataControlSourceV1,
    wayland_protocols_wlr::data_control::v1::client,
    zwlr_data_control_device_v1,
    zwlr_data_control_source_v1,
    wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1
);

fn serve(data: Arc<Vec<u8>>, fd: OwnedFd) {
    std::thread::spawn(move || write_paste(std::fs::File::from(fd), &data));
}

/// Answer a paste: write all of `data` to the pipe the pasting app gave.
/// Some apps (Chromium-based ones such as Edge and Teams) make that pipe
/// non-blocking, and then a plain `write_all` stops with "would block" once
/// the pipe's 64 KB buffer fills, so the app got a cut-off PNG: the top of
/// the image and grey below. Our end is made blocking first, so the write
/// waits for the app to read. The flag is on the write end, which only we
/// use.
pub fn write_paste(mut pipe: impl Write + std::os::fd::AsFd, data: &[u8]) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    if let Ok(flags) = fcntl_getfl(&pipe)
        && flags.contains(OFlags::NONBLOCK)
    {
        let _ = fcntl_setfl(&pipe, flags - OFlags::NONBLOCK);
    }
    // An app that stops reading early (it cancelled the paste) closes its
    // end; that's normal, so it's only reported when debugging.
    if let Err(e) = pipe.write_all(data)
        && std::env::var_os("CLIPT_DEBUG").is_some()
    {
        eprintln!("clipt: paste of {} bytes stopped: {e}", data.len());
    }
}

/// Whether the compositor offers a data-control protocol, so
/// `copy_and_serve` can work. GNOME doesn't.
pub fn has_data_control() -> bool {
    let Ok(conn) = crate::capture::wayland::connection() else { return false };
    let Ok((globals, _)) = registry_queue_init::<State>(&conn) else { return false };
    globals
        .contents()
        .with_list(|l| l.iter().any(|g| g.interface == "ext_data_control_manager_v1" || g.interface == "zwlr_data_control_manager_v1"))
}

/// Claim the clipboard and block until someone else replaces it.
pub fn copy_and_serve(png: Vec<u8>, before_serving: impl FnOnce()) -> Result<()> {
    let conn = crate::capture::wayland::connection()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
    let mut state = State { data: Arc::new(png), done: false };
    ext::set(&globals, &qh, &seat, "image/png")
        .or_else(|| wlr::set(&globals, &qh, &seat, "image/png"))
        .context("compositor has no data-control protocol (GNOME?) — pipe `-o -` into wl-copy instead")?;
    queue.roundtrip(&mut state)?;
    before_serving();
    while !state.done {
        queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::pipe::{PipeFlags, pipe_with};
    use std::io::Read;

    /// A non-blocking pipe, read slowly: all of a PNG larger than the pipe's
    /// buffer arrives. A plain `write_all` stops at the buffer's size.
    #[test]
    fn a_non_blocking_pipe_gets_everything() {
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let (read, write) = pipe_with(PipeFlags::NONBLOCK).unwrap();
        let mut plain = std::fs::File::from(write.try_clone().unwrap());
        assert!(plain.write_all(&data).is_err(), "a plain write_all gives up on a full non-blocking pipe");
        drop((plain, write));
        // Drain what the failed write left, on a fresh pipe for the real test.
        drop(read);

        let (read, write) = pipe_with(PipeFlags::NONBLOCK).unwrap();
        let expect = data.clone();
        let writer = std::thread::spawn(move || write_paste(std::fs::File::from(write), &data));
        let mut reader = std::fs::File::from(read);
        rustix::fs::fcntl_setfl(&reader, rustix::fs::OFlags::empty()).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            std::thread::sleep(std::time::Duration::from_micros(200));
            match reader.read(&mut buf).unwrap() {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        writer.join().unwrap();
        assert_eq!(got.len(), expect.len());
        assert!(got == expect);
    }
}
