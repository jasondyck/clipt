//! Recording on Wayland and X11: which capture to use, and the frame and
//! window following that go with it.

use super::{Feed, Options, Sink, Target, debug, follow, indicator, resolve_target};
use crate::capture::wayland::{Protocol, Wayland};
use crate::capture::{Backend, LogicalRect, Output};
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where frames come from.
enum Source {
    /// An output, cropped to `crop` (output-logical; updated while following
    /// a window).
    Output { out: Output, crop: Arc<Mutex<Option<LogicalRect>>> },
    /// One window, captured by the compositor itself.
    Window { app: String, title: String },
    /// KDE: a screencast from KWin over PipeWire.
    #[cfg(feature = "pipewire")]
    Kde(crate::capture::kwin_cast::Target),
    /// GNOME: a screencast from Mutter over PipeWire.
    #[cfg(feature = "pipewire")]
    Gnome(crate::capture::mutter_cast::Target),
    /// The ScreenCast portal: what the user picks, cropped to a desktop region.
    #[cfg(feature = "pipewire")]
    Portal(crate::capture::portal_cast::Kind, Option<LogicalRect>),
    /// X11: an output, cropped to `crop` (updated while following a window).
    X11 { out: Output, crop: Arc<Mutex<Option<LogicalRect>>> },
}

/// How this desktop lets us record.
enum Method {
    Wlroots(Protocol),
    #[cfg(feature = "pipewire")]
    Kde,
    #[cfg(feature = "pipewire")]
    Gnome,
    #[cfg(feature = "pipewire")]
    Portal,
}

/// Pick how to record `opts.target` here, and set it up.
pub fn feed(opts: &Options, interval: Duration) -> Result<Feed> {
    // A Wayland compositor if there is one, else an X display.
    let (wl, x11) = match Wayland::connect() {
        Ok(wl) => (Some(wl), None),
        Err(e) if crate::x11::available() => {
            let x = crate::x11::X11::connect().map_err(|x| x.context(format!("and Wayland: {e:#}")))?;
            (None, Some(x))
        }
        Err(e) => return Err(e.context("no Wayland compositor and no X11 display")),
    };
    let outputs = match (&wl, &x11) {
        (Some(wl), _) => wl.outputs(),
        (_, Some(x)) => x.outputs(),
        _ => unreachable!(),
    };
    anyhow::ensure!(!outputs.is_empty(), "no outputs found");
    // `zoom`: frames change size, so they go through the camera.
    let (source, zoom, follow, frame, proto) = match &wl {
        Some(w) => {
            let wlroots = w.protocols();
            let method = pick_method(w, &wlroots, opts.backend)?;
            #[cfg(feature = "pipewire")]
            if matches!(method, Method::Kde | Method::Gnome | Method::Portal) {
                crate::capture::pipewire::check()?;
            }
            let proto = match method {
                Method::Wlroots(p) => p,
                #[allow(unreachable_patterns)]
                _ => Protocol::ExtImageCopy,
            };
            let (s, z, f, fr) = match method {
                Method::Wlroots(p) => wlroots_source(w, p, &outputs, &opts.target)?,
                #[cfg(feature = "pipewire")]
                Method::Kde => kde_source(&outputs, &opts.target)?,
                #[cfg(feature = "pipewire")]
                Method::Gnome => gnome_source(&outputs, &opts.target)?,
                #[cfg(feature = "pipewire")]
                Method::Portal => portal_source(&outputs, &opts.target)?,
            };
            (s, z, f, fr, proto)
        }
        None => {
            if let Some(b) = opts.backend.filter(|b| *b != Backend::X11) {
                bail!("this is an X11 session; recording uses X11 here, not {b:?}");
            }
            let (s, z, f, fr) = x11_source(&outputs, &opts.target)?;
            (s, z, f, fr, Protocol::ExtImageCopy)
        }
    };
    let on_x11 = x11.is_some();
    let follow_crop = match &source {
        Source::Output { crop, .. } | Source::X11 { crop, .. } => Some(crop.clone()),
        _ => None,
    };
    let start = Box::new(move || {
        if let Some((out, c)) = frame {
            std::thread::spawn(move || {
                if let Err(e) = indicator::run(&out, c) {
                    eprintln!("clipt: no recording frame: {e:#}");
                }
            });
        }
        if let (Some(crop), Some((id, out))) = (follow_crop, follow) {
            std::thread::spawn(move || follow::run(&id, &out, &crop, on_x11));
        }
    });
    let cursor = opts.cursor;
    let (mut wl, x11) = (wl, x11);
    let stream = Box::new(move |mut sink: Sink| match &source {
        Source::Output { out, crop } => wl.as_mut().unwrap().stream(out, cursor, proto, &|| *crop.lock().unwrap(), &mut sink),
        Source::Window { app, title } => wl.as_mut().unwrap().stream_window(app, title, cursor, &mut sink),
        Source::X11 { out, crop } => x11.as_ref().unwrap().stream(out, cursor, interval, &|| *crop.lock().unwrap(), &mut sink),
        #[cfg(feature = "pipewire")]
        Source::Kde(t) => crate::capture::kwin_cast::stream(t.clone(), cursor, Box::new(|_, _| None), sink),
        #[cfg(feature = "pipewire")]
        Source::Gnome(t) => crate::capture::mutter_cast::stream(t.clone(), cursor, Box::new(|_, _| None), sink),
        #[cfg(feature = "pipewire")]
        Source::Portal(kind, region) => crate::capture::portal_cast::stream(*kind, *region, cursor, sink),
    });
    Ok(Feed { stream, zoom, start })
}

/// How to record here: a wlroots capture protocol where the compositor has
/// one, else KWin's screencast (KDE), else Mutter's (GNOME).
fn pick_method(wl: &Wayland, wlroots: &[Protocol], forced: Option<Backend>) -> Result<Method> {
    #[cfg(feature = "pipewire")]
    let (kde, gnome, portal) = (
        || crate::capture::kwin_cast::available(wl.connection()),
        || crate::dbus::session().is_ok_and(|c| crate::capture::mutter_cast::available(&c)),
        || crate::dbus::session().is_ok_and(|c| crate::capture::portal_cast::available(&c)),
    );
    let _ = wl;
    let ext = wlroots.contains(&Protocol::ExtImageCopy);
    Ok(match forced {
        Some(Backend::Ext) => {
            anyhow::ensure!(ext, "compositor doesn't offer ext-image-copy-capture");
            Method::Wlroots(Protocol::ExtImageCopy)
        }
        Some(Backend::Wlr) => {
            anyhow::ensure!(wlroots.contains(&Protocol::WlrScreencopy), "compositor doesn't offer wlr-screencopy");
            Method::Wlroots(Protocol::WlrScreencopy)
        }
        #[cfg(feature = "pipewire")]
        Some(Backend::Kwin) => {
            anyhow::ensure!(kde(), "KWin's screencast isn't available here");
            Method::Kde
        }
        #[cfg(feature = "pipewire")]
        Some(Backend::Mutter) => {
            anyhow::ensure!(gnome(), "Mutter's screencast isn't available here");
            Method::Gnome
        }
        #[cfg(feature = "pipewire")]
        Some(Backend::Portal) => {
            anyhow::ensure!(portal(), "the ScreenCast portal isn't available here");
            Method::Portal
        }
        Some(b) => bail!("recording doesn't support the {b:?} backend (ext, wlr, kwin, mutter or portal)"),
        None if ext => Method::Wlroots(Protocol::ExtImageCopy),
        None if !wlroots.is_empty() => Method::Wlroots(Protocol::WlrScreencopy),
        #[cfg(feature = "pipewire")]
        None if kde() => Method::Kde,
        #[cfg(feature = "pipewire")]
        None if gnome() => Method::Gnome,
        #[cfg(feature = "pipewire")]
        None if portal() => Method::Portal,
        None if cfg!(feature = "pipewire") => {
            bail!("this desktop offers no way to record: no wlroots capture protocol, no KWin or Mutter screencast")
        }
        None => bail!("this desktop needs PipeWire to record, and this clipt was built without it"),
    })
}

/// (source, zoom, window to follow, red frame to draw: output + crop)
type Picked = (Source, bool, Option<(String, Output)>, Option<(Output, LogicalRect)>);

/// wlroots compositors: capture the output (cropped) or the window directly.
fn wlroots_source(wl: &Wayland, proto: Protocol, outputs: &[Output], target: &Target) -> Result<Picked> {
    let rect = |w: &crate::windows::Window| Target::Rect((w.x as f64, w.y as f64, w.width as f64, w.height as f64));
    Ok(match target {
        Target::Window(w) if wl.can_capture_windows() && proto == Protocol::ExtImageCopy && !w.app.is_empty() => {
            debug(&format!("recording window '{}' by window capture", w.app));
            (Source::Window { app: w.app.clone(), title: w.title.clone() }, true, None, None)
        }
        Target::Window(w) if follow::supported() && !w.id.is_empty() => {
            let (out, crop) = resolve_target(outputs, &rect(w))?;
            debug("following the window by cropping its output");
            (Source::Output { out: out.clone(), crop: Arc::new(Mutex::new(crop)) }, true, Some((w.id.clone(), out)), None)
        }
        Target::Window(w) => {
            eprintln!("clipt: this compositor can't follow a window; recording the area it covers now");
            let (out, crop) = resolve_target(outputs, &rect(w))?;
            let frame = crop.map(|c| (out.clone(), c));
            (Source::Output { out, crop: Arc::new(Mutex::new(crop)) }, false, None, frame)
        }
        t => {
            let (out, crop) = resolve_target(outputs, t)?;
            let frame = crop.map(|c| (out.clone(), c));
            (Source::Output { out, crop: Arc::new(Mutex::new(crop)) }, false, None, frame)
        }
    })
}

/// X11: the output (cropped), or a window followed by its position. No red
/// frame: drawing one needs override-redirect windows, which would show in
/// the recording whenever the region touches them.
fn x11_source(outputs: &[Output], target: &Target) -> Result<Picked> {
    let rect = |w: &crate::windows::Window| Target::Rect((w.x as f64, w.y as f64, w.width as f64, w.height as f64));
    Ok(match target {
        Target::Window(w) if !w.id.is_empty() => {
            let (out, crop) = resolve_target(outputs, &rect(w))?;
            debug("following the X11 window by its position");
            (Source::X11 { out: out.clone(), crop: Arc::new(Mutex::new(crop)) }, true, Some((w.id.clone(), out)), None)
        }
        t => {
            let (out, crop) = resolve_target(outputs, t)?;
            (Source::X11 { out, crop: Arc::new(Mutex::new(crop)) }, false, None, None)
        }
    })
}

/// The ScreenCast portal: the user picks a monitor (or window) in its dialog,
/// once; a region is cropped out of the monitor they pick.
#[cfg(feature = "pipewire")]
fn portal_source(outputs: &[Output], target: &Target) -> Result<Picked> {
    use crate::capture::portal_cast::Kind;
    Ok(match target {
        Target::Screen(_) => (Source::Portal(Kind::Monitor, None), false, None, None),
        Target::Window(_) => (Source::Portal(Kind::Window, None), true, None, None),
        t => {
            let (o, crop) = resolve_target(outputs, t)?;
            let region = crop.map(|(x, y, w, h)| (x + o.x as f64, y + o.y as f64, w, h));
            (Source::Portal(Kind::Monitor, region), false, None, None)
        }
    })
}

/// GNOME: Mutter records the monitor or area and hands it over by PipeWire.
/// GNOME doesn't say where windows are, so there are no window targets.
#[cfg(feature = "pipewire")]
fn gnome_source(outputs: &[Output], target: &Target) -> Result<Picked> {
    use crate::capture::mutter_cast::Target as G;
    Ok(match target {
        Target::Screen(None) => (Source::Gnome(G::Monitor(outputs[0].name.clone())), false, None, None),
        Target::Screen(Some(n)) => {
            anyhow::ensure!(outputs.iter().any(|o| &o.name == n), "no output named '{n}'");
            (Source::Gnome(G::Monitor(n.clone())), false, None, None)
        }
        t => {
            let (o, crop) = resolve_target(outputs, t)?;
            let c = crop.unwrap_or((0.0, 0.0, o.width as f64, o.height as f64));
            // The frame comes from clipt's GNOME Shell extension, if it's there.
            (Source::Gnome(G::Area((c.0 + o.x as f64, c.1 + o.y as f64, c.2, c.3))), false, None, crop.map(|c| (o, c)))
        }
    })
}

/// KDE: KWin records the output, region or window and hands it over by PipeWire.
#[cfg(feature = "pipewire")]
fn kde_source(outputs: &[Output], target: &Target) -> Result<Picked> {
    use crate::capture::kwin_cast::Target as K;
    let scale = |o: &Output| if o.native.0 > 0 { o.native.0 as f64 / o.width as f64 } else { 1.0 };
    Ok(match target {
        Target::Screen(None) => (Source::Kde(K::Output(outputs[0].name.clone())), false, None, None),
        Target::Screen(Some(n)) => {
            anyhow::ensure!(outputs.iter().any(|o| &o.name == n), "no output named '{n}'");
            (Source::Kde(K::Output(n.clone())), false, None, None)
        }
        Target::Window(w) if !w.id.is_empty() => {
            debug(&format!("recording window '{}' by KWin screencast", w.app));
            (Source::Kde(K::Window(w.id.clone())), true, None, None)
        }
        t => {
            let (o, crop) = resolve_target(outputs, t)?;
            let c = crop.unwrap_or((0.0, 0.0, o.width as f64, o.height as f64));
            let source = Source::Kde(K::Region((c.0 + o.x as f64, c.1 + o.y as f64, c.2, c.3), scale(&o)));
            (source, false, None, Some((o, c)))
        }
    })
}
