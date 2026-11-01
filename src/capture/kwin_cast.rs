//! KDE recording: KWin's zkde_screencast protocol hands out a PipeWire node
//! for an output, a region of the desktop or one window, which
//! `capture::pipewire` then reads.

use super::LogicalRect;
use super::pipewire::{CropFn, Remote};
use crate::capture::VideoFrame;
use anyhow::{Context, Result, bail};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry},
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self as zstream, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::{Pointer, ZkdeScreencastUnstableV1},
};

#[derive(Clone)]
pub enum Target {
    /// An output, by name.
    Output(String),
    /// A desktop region in logical coordinates, recorded at `scale`.
    Region(LogicalRect, f64),
    /// A window, by KWin's internal id (a UUID).
    Window(String),
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, String)>,
    node: Option<u32>,
    failed: Option<String>,
}

/// KWin offers screencasting to this client.
pub fn available(conn: &Connection) -> bool {
    registry_queue_init::<State>(conn)
        .map(|(g, _)| g.contents().with_list(|l| l.iter().any(|g| g.interface == "zkde_screencast_unstable_v1")))
        .unwrap_or(false)
}

/// Record `target` until `sink` returns false.
pub fn stream(target: Target, cursor: bool, crop: CropFn, sink: Box<dyn FnMut(VideoFrame) -> bool>) -> Result<()> {
    let conn = crate::capture::wayland::connection()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let cast: ZkdeScreencastUnstableV1 = globals.bind(&qh, 1..=4, ()).context("KWin doesn't offer screencasting (zkde_screencast)")?;
    let mut state = State::default();
    for g in globals.contents().clone_list() {
        if g.interface == "wl_output" && g.version >= 4 {
            let o: wl_output::WlOutput = globals.registry().bind(g.name, 4, &qh, ());
            state.outputs.push((o, String::new()));
        }
    }
    queue.roundtrip(&mut state)?;
    let pointer = if cursor { Pointer::Embedded } else { Pointer::Hidden } as u32;
    let s = match &target {
        Target::Output(name) => {
            let o = state
                .outputs
                .iter()
                .find(|(_, n)| n == name)
                .map(|(o, _)| o.clone())
                .with_context(|| format!("no output named '{name}'"))?;
            cast.stream_output(&o, pointer, &qh, ())
        }
        &Target::Region((x, y, w, h), scale) => {
            anyhow::ensure!(cast.version() >= 3, "this KWin can't record a region (zkde_screencast v{} < 3)", cast.version());
            cast.stream_region(
                x.round() as i32,
                y.round() as i32,
                w.round().max(1.0) as u32,
                h.round().max(1.0) as u32,
                scale,
                pointer,
                &qh,
                (),
            )
        }
        Target::Window(uuid) => cast.stream_window(uuid.clone(), pointer, &qh, ()),
    };
    while state.node.is_none() && state.failed.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    if let Some(e) = state.failed {
        bail!("KWin refused the screencast: {e}");
    }
    let node = state.node.unwrap();
    debug(&format!("KWin screencast node {node}"));
    // `conn`, `cast` and `s` stay alive while PipeWire runs: dropping the
    // stream object would end the screencast.
    let r = super::pipewire::stream(Remote { fd: None, node }, crop, sink);
    s.close();
    let _ = conn.flush();
    r
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(s: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: zstream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zstream::Event::Created { node } => s.node = Some(node),
            zstream::Event::Failed { error } => s.failed = Some(error),
            zstream::Event::Closed => {
                s.failed.get_or_insert_with(|| "the stream closed".into());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev
            && let Some(e) = s.outputs.iter_mut().find(|(x, _)| x == o)
        {
            e.1 = name;
        }
    }
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

wayland_client::delegate_noop!(State: ZkdeScreencastUnstableV1);

fn debug(msg: &str) {
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {msg}");
    }
}
