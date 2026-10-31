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
    std::thread::spawn(move || {
        let mut f = std::fs::File::from(fd);
        let _ = f.write_all(&data);
    });
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
