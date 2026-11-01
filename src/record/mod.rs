//! `clipt record`: screen and region recording to MP4.
//!
//! A capture thread streams frames from the compositor and keeps only the
//! newest one; the main thread hands frames to ffmpeg at most `fps` times a
//! second. A slow encoder therefore drops frames rather than queueing them.
//!
//! One recording runs at a time. Its pid is in
//! `$XDG_RUNTIME_DIR/clipt-record.pid`; `clipt record --stop` (and Ctrl-C,
//! SIGTERM) end it cleanly, letting ffmpeg finish the file.
//!
//! See docs/recording-plan.md for what's still to come (overlay, windows,
//! PipeWire for KDE/GNOME, X11).

mod camera;
mod encode;
mod follow;
mod indicator;
mod mkv;

use crate::capture::wayland::{Protocol, Wayland};
use crate::capture::{Backend, LogicalRect, Output, VideoFrame};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub enum Target {
    /// One output, by name (default: the first).
    Screen(Option<String>),
    /// Slurp-style "X,Y WxH" in logical desktop coordinates; must lie
    /// within one output.
    Geometry(String),
    /// A selection from the overlay, in logical desktop coordinates. Trimmed
    /// to the output under its centre.
    Rect(LogicalRect),
    /// A window picked in the overlay: followed as it moves and resizes
    /// where the compositor allows.
    Window(crate::windows::Window),
}

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

pub struct Options {
    pub target: Target,
    pub output: Option<String>,
    pub fps: Option<u32>,
    pub cursor: bool,
    pub backend: Option<Backend>,
    /// Save a GIF (also chosen by a `.gif` output name or `record_format`).
    pub gif: bool,
}

/// Stop recordings below this much free space, before ffmpeg hits ENOSPC
/// and leaves an unplayable file.
const MIN_FREE_BYTES: u64 = 500 << 20;

static STOP: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
/// When the last pause, resume and stop signals arrived (monotonic clock,
/// ns). The writer may be busy handing ffmpeg a frame when one arrives, so
/// it uses these rather than the moment it notices.
static PAUSED_AT: AtomicU64 = AtomicU64::new(0);
static RESUMED_AT: AtomicU64 = AtomicU64::new(0);
static STOPPED_AT: AtomicU64 = AtomicU64::new(0);

fn monotonic_ns() -> u64 {
    // clock_gettime is async-signal-safe, so the handler may call this.
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// The Instant matching a monotonic_ns() reading (0 = now).
fn instant_at(ns: u64) -> Instant {
    let ago = monotonic_ns().saturating_sub(ns);
    if ns == 0 { Instant::now() } else { Instant::now().checked_sub(Duration::from_nanos(ago)).unwrap_or_else(Instant::now) }
}

extern "C" fn on_signal(sig: libc::c_int) {
    apply(match sig {
        libc::SIGUSR1 => CMD_PAUSE,
        libc::SIGUSR2 => CMD_RESUME,
        _ => CMD_STOP,
    });
}

/// Act on a command from the control FIFO or a signal. Async-signal-safe.
fn apply(cmd: u8) {
    // The flag to set, its new value, and where to note when it changed.
    let (flag, value, at) = match cmd {
        CMD_PAUSE => (&PAUSED, true, &PAUSED_AT),
        CMD_RESUME => (&PAUSED, false, &RESUMED_AT),
        CMD_STOP => (&STOP, true, &STOPPED_AT),
        _ => return,
    };
    if flag.swap(value, Ordering::SeqCst) != value {
        at.store(monotonic_ns(), Ordering::SeqCst);
    }
}

// Recording control works through files in the runtime dir rather than
// pids: each Flatpak instance has its own PID namespace (where the first
// process is always pid 2), so a pid in the pidfile means nothing to another
// `flatpak run`, and signalling it would hit the wrong process.
//   clipt-record.pid  held under an exclusive flock while recording (locks
//                     cross PID namespaces); says "recording" or "paused"
//   clipt-record.ctl  a FIFO the recorder reads one-byte commands from

fn runtime_file(name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(name)
}

fn pidfile() -> PathBuf {
    runtime_file("clipt-record.pid")
}

fn ctlfile() -> PathBuf {
    runtime_file("clipt-record.ctl")
}

const CMD_STOP: u8 = b's';
const CMD_PAUSE: u8 = b'p';
const CMD_RESUME: u8 = b'r';

/// The running recording, if any: whether it's paused. The pidfile holds the
/// recorder's pid (for people reading it) on the first line and "paused" or
/// "recording" on the second.
pub fn status() -> Option<bool> {
    use rustix::fs::{FlockOperation, flock};
    let f = std::fs::File::open(pidfile()).ok()?;
    match flock(&f, FlockOperation::NonBlockingLockShared) {
        Ok(()) => None, // nobody holds it: stale
        Err(_) => {
            let s = std::io::read_to_string(&f).unwrap_or_default();
            Some(s.lines().nth(1) == Some("paused"))
        }
    }
}

/// Holds the pidfile (locked) and the control FIFO for as long as the
/// recording runs.
struct Lock {
    file: std::fs::File,
}

impl Lock {
    fn take() -> Result<Self> {
        use rustix::fs::{FlockOperation, flock};
        let p = pidfile();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)
            .with_context(|| format!("opening {}", p.display()))?;
        if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
            bail!("already recording; stop it with 'clipt record --stop'");
        }
        let lock = Self { file };
        lock.set(false)?;
        let ctl = ctlfile();
        let _ = std::fs::remove_file(&ctl); // stale
        rustix::fs::mkfifoat(rustix::fs::CWD, &ctl, rustix::fs::Mode::from_raw_mode(0o600))
            .with_context(|| format!("creating {}", ctl.display()))?;
        // Read and write: with a writer of its own, reads wait for the next
        // command instead of seeing end-of-file whenever a sender closes.
        let mut fifo =
            std::fs::OpenOptions::new().read(true).write(true).open(&ctl).with_context(|| format!("opening {}", ctl.display()))?;
        std::thread::spawn(move || {
            let mut b = [0u8; 16];
            while let Ok(n) = std::io::Read::read(&mut fifo, &mut b) {
                b[..n].iter().for_each(|&c| apply(c));
            }
        });
        Ok(lock)
    }

    fn set(&self, paused: bool) -> Result<()> {
        use std::os::unix::fs::FileExt;
        // Rewritten in place: replacing the file would drop the lock.
        let s = format!("{}\n{}\n", std::process::id(), if paused { "paused" } else { "recording" });
        self.file.set_len(0)?;
        self.file.write_all_at(s.as_bytes(), 0)?;
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(ctlfile());
        let _ = std::fs::remove_file(pidfile());
    }
}

/// Fail early, before the overlay opens, if a recording is already running.
pub fn ensure_idle() -> Result<()> {
    match status() {
        Some(_) => bail!("already recording; stop it with 'clipt record --stop'"),
        None => Ok(()),
    }
}

/// Hand the running recording a command.
fn send(cmd: u8) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if status().is_none() {
        bail!("no recording is running");
    }
    // Non-blocking, so a FIFO nobody reads fails at once instead of hanging.
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(ctlfile())
        .context("no recording is running (its control FIFO isn't open)")?;
    fifo.write_all(&[cmd]).context("sending the command to the recording")?;
    Ok(())
}

/// `clipt record --stop`: tell the running recording to stop and wait for it
/// to finish writing.
pub fn stop() -> Result<()> {
    send(CMD_STOP)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while status().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(status().is_none(), "the recording didn't stop within 30 s");
    Ok(())
}

pub enum PauseCmd {
    Pause,
    Resume,
    Toggle,
}

/// `clipt record --pause / --resume / --toggle-pause`.
pub fn pause(cmd: PauseCmd) -> Result<()> {
    let Some(paused) = status() else { bail!("no recording is running") };
    let pause = match cmd {
        PauseCmd::Pause => true,
        PauseCmd::Resume => false,
        PauseCmd::Toggle => !paused,
    };
    send(if pause { CMD_PAUSE } else { CMD_RESUME })
}

/// The newest captured frame, shared between the capture thread and the writer.
#[derive(Default)]
struct Slot {
    /// The frame and when it was captured.
    frame: Option<(VideoFrame, Instant)>,
    error: Option<String>,
    done: bool,
}

pub fn run(cfg: &Config, opts: Options) -> Result<()> {
    let fps = opts.fps.unwrap_or(cfg.record_fps).clamp(1, 240);
    let codec = encode::choose(&cfg.record_codec)?;

    // A Wayland compositor if there is one, else an X display.
    let (mut wl, x11) = match Wayland::connect() {
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
    let interval = Duration::from_secs_f64(1.0 / fps as f64);

    let gif_format = match cfg.record_format.as_str() {
        "mp4" => opts.gif,
        "gif" => true,
        other => bail!("unknown record_format '{other}' (mp4, gif)"),
    };
    let path = match &opts.output {
        Some(p) => crate::config::expand(p),
        None => cfg.next_video_path(if gif_format { "gif" } else { "mp4" }),
    };
    check_extension(&path)?;
    // A GIF is made from an MP4 when the recording stops: its palette needs
    // every frame, and ffmpeg would otherwise hold them all in memory.
    let gif = is_gif(&path);
    let target = path.clone();
    let path = if gif { hidden_sibling(&path, "recording.mp4") } else { path };
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }

    let lock = Lock::take()?;
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1, libc::SIGUSR2] {
            libc::signal(sig, on_signal as *const () as libc::sighandler_t);
        }
    }

    if let Some((out, c)) = frame {
        std::thread::spawn(move || {
            if let Err(e) = indicator::run(&out, c) {
                eprintln!("clipt: no recording frame: {e:#}");
            }
        });
    }
    if let (Source::Output { crop, .. } | Source::X11 { crop, .. }, Some((id, out))) = (&source, &follow) {
        let (crop, id, out, on_x11) = (crop.clone(), id.clone(), out.clone(), x11.is_some());
        std::thread::spawn(move || follow::run(&id, &out, &crop, on_x11));
    }

    let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
    let captured = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let (slot, captured, cursor) = (slot.clone(), captured.clone(), opts.cursor);
        // The thread may still be blocked waiting for the screen to change
        // when we finish; process exit ends it.
        std::thread::spawn(move || {
            let frames = slot.clone();
            let mut sink = move |f: VideoFrame| {
                captured.fetch_add(1, Ordering::Relaxed);
                let (m, cv) = &*frames;
                m.lock().unwrap().frame = Some((f, Instant::now()));
                cv.notify_one();
                !STOP.load(Ordering::SeqCst)
            };
            let r = match &source {
                Source::Output { out, crop } => wl.as_mut().unwrap().stream(out, cursor, proto, &|| *crop.lock().unwrap(), &mut sink),
                Source::Window { app, title } => wl.as_mut().unwrap().stream_window(app, title, cursor, &mut sink),
                Source::X11 { out, crop } => x11.as_ref().unwrap().stream(out, cursor, interval, &|| *crop.lock().unwrap(), &mut sink),
                #[cfg(feature = "pipewire")]
                Source::Kde(t) => crate::capture::kwin_cast::stream(t.clone(), cursor, Box::new(|_, _| None), Box::new(sink)),
                #[cfg(feature = "pipewire")]
                Source::Gnome(t) => crate::capture::mutter_cast::stream(t.clone(), cursor, Box::new(|_, _| None), Box::new(sink)),
                #[cfg(feature = "pipewire")]
                Source::Portal(kind, region) => crate::capture::portal_cast::stream(*kind, *region, cursor, Box::new(sink)),
            };
            let (m, cv) = &*slot;
            let mut s = m.lock().unwrap();
            s.done = true;
            if let Err(e) = r {
                s.error = Some(format!("{e:#}"));
            }
            cv.notify_one();
        });
    }

    let mut result = write_loop(cfg, &lock, &slot, &codec, fps, zoom, &path, &captured);
    if gif && result.is_ok() {
        result = encode::to_gif(&path, &target, fps.min(15)).map(|()| {
            let _ = std::fs::remove_file(&path);
            "GIF".to_string()
        });
    }
    match &result {
        Ok(stats) => {
            println!("{}", target.display());
            if cfg.notify {
                crate::notify::send("Recording saved", &format!("{}  ·  {}", target.display(), stats), None);
            }
        }
        Err(_) if path.exists() => eprintln!("clipt: the partial recording is in {}", path.display()),
        Err(_) => {}
    }
    result.map(|_| ())
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
        Some(Backend::Portal) => {
            anyhow::ensure!(portal(), "the ScreenCast portal isn't available here");
            Method::Portal
        }
        Some(b) => bail!("recording doesn't support the {b:?} backend (ext, wlr, kwin or portal)"),
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
            // No frame: GNOME has no layer-shell to draw it with.
            (Source::Gnome(G::Area((c.0 + o.x as f64, c.1 + o.y as f64, c.2, c.3))), false, None, None)
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

/// The output to capture and the crop within it (output-logical coordinates).
fn resolve_target(outputs: &[Output], target: &Target) -> Result<(Output, Option<LogicalRect>)> {
    match target {
        Target::Screen(None) => Ok((outputs[0].clone(), None)),
        Target::Screen(Some(n)) => {
            Ok((outputs.iter().find(|o| &o.name == n).with_context(|| format!("no output named '{n}'"))?.clone(), None))
        }
        Target::Geometry(g) => {
            let (x, y, w, h) = crate::parse_geometry(g)?;
            let inside = |o: &&Output| {
                let (ox, oy) = (o.x as f64, o.y as f64);
                x >= ox && y >= oy && x + w <= ox + o.width as f64 && y + h <= oy + o.height as f64
            };
            let Some(o) = outputs.iter().find(inside) else {
                bail!("geometry '{g}' must lie within one output (recording across monitors isn't supported yet)")
            };
            Ok((o.clone(), Some((x - o.x as f64, y - o.y as f64, w, h))))
        }
        Target::Window(w) => resolve_target(outputs, &Target::Rect((w.x as f64, w.y as f64, w.width as f64, w.height as f64))),
        &Target::Rect((x, y, w, h)) => {
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            let o = outputs
                .iter()
                .find(|o| {
                    let (ox, oy) = (o.x as f64, o.y as f64);
                    cx >= ox && cy >= oy && cx < ox + o.width as f64 && cy < oy + o.height as f64
                })
                .context("the selection's centre isn't on any output")?;
            let (ox, oy) = (o.x as f64, o.y as f64);
            let (l, t) = (x.max(ox), y.max(oy));
            let (r, b) = ((x + w).min(ox + o.width as f64), (y + h).min(oy + o.height as f64));
            anyhow::ensure!(r - l >= 1.0 && b - t >= 1.0, "the selection is empty");
            Ok((o.clone(), Some((l - ox, t - oy, r - l, b - t))))
        }
    }
}

fn check_extension(path: &Path) -> Result<()> {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mp4" | "mkv" | "mov" | "gif") => Ok(()),
        _ => bail!("{}: recordings are .mp4, .mkv, .mov or .gif", path.display()),
    }
}

fn is_gif(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// `.<name>.<suffix>` next to `path`, for intermediate files.
fn hidden_sibling(path: &Path, suffix: &str) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("recording");
    path.with_file_name(format!(".{stem}.{suffix}"))
}

/// Feed frames to ffmpeg until stopped; returns a short summary.
///
/// Each frame is stamped with when it was captured, minus the time spent
/// paused before it, so paused time is simply left out of the video.
#[allow(clippy::too_many_arguments)]
fn write_loop(
    cfg: &Config,
    lock: &Lock,
    slot: &Arc<(Mutex<Slot>, Condvar)>,
    codec: &encode::Codec,
    fps: u32,
    zoom: bool,
    path: &Path,
    captured: &std::sync::atomic::AtomicU64,
) -> Result<String> {
    let (m, cv) = &**slot;
    // Window recordings: frames of changing size go through the camera.
    let mut cam: Option<camera::Camera> = None;
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let started = Instant::now();
    let max = (cfg.max_duration > 0).then(|| Duration::from_secs(cfg.max_duration));
    let mut enc: Option<encode::Encoder> = None;
    // Video time zero, and the paused time to leave out after it.
    let mut t0: Option<Instant> = None;
    let mut paused_total = Duration::ZERO;
    let mut paused_since: Option<Instant> = None;
    let mut last: Option<VideoFrame> = None;
    // `last` arrived after the most recent write and still needs sending,
    // stamped no earlier than `last_at`.
    let mut fresh = false;
    let mut last_at = Instant::now();
    let mut last_write = Instant::now();
    let mut last_ts: Option<u64> = None;
    let mut written = 0u64;
    let mut last_disk_check = Instant::now();
    let mut stop_reason: Option<String> = None;

    // Milliseconds into the video for a moment `at`, kept strictly increasing.
    let stamp = |at: Instant, t0: Instant, paused: Duration, last_ts: &mut Option<u64>| -> u64 {
        let ms = at.saturating_duration_since(t0).saturating_sub(paused).as_millis() as u64;
        let ms = match *last_ts {
            Some(prev) if ms <= prev => prev + 1,
            _ => ms,
        };
        *last_ts = Some(ms);
        ms
    };

    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        if max.is_some_and(|m| started.elapsed() >= m) {
            stop_reason = Some(format!("reached max_duration ({} s)", cfg.max_duration));
            break;
        }
        if last_disk_check.elapsed() >= Duration::from_secs(5) {
            last_disk_check = Instant::now();
            if free_bytes(path).is_some_and(|b| b < MIN_FREE_BYTES) {
                stop_reason = Some("less than 500 MB of disk space left".into());
                break;
            }
        }
        let paused = PAUSED.load(Ordering::SeqCst);
        match (paused, paused_since) {
            (true, None) => {
                paused_since = Some(instant_at(PAUSED_AT.load(Ordering::SeqCst)));
                let _ = lock.set(true);
            }
            (false, Some(since)) => {
                let resumed = instant_at(RESUMED_AT.load(Ordering::SeqCst)).max(since);
                paused_total += resumed - since;
                paused_since = None;
                // Show whatever is on screen now from the moment we resume.
                fresh = last.is_some();
                last_at = resumed;
                let _ = lock.set(false);
            }
            _ => {}
        }

        // Take the newest frame. While one is waiting to be sent, wait no
        // longer than the rest of the frame interval.
        let animating = cam.as_ref().zip(last.as_ref()).is_some_and(|(c, f)| c.animating(f.width, f.height));
        let wait = if (fresh || animating) && !paused { interval.saturating_sub(last_write.elapsed()) } else { Duration::from_millis(100) };
        {
            let mut s = m.lock().unwrap();
            if s.frame.is_none() && !s.done && !wait.is_zero() {
                s = cv.wait_timeout(s, wait).unwrap().0;
            }
            if let Some(e) = s.error.take() {
                drop(s);
                if let Some(e2) = enc.take() {
                    let _ = e2.finish();
                }
                bail!("capture failed: {e}");
            }
            if s.done && s.frame.is_none() {
                break;
            }
            if let Some((f, at)) = s.frame.take() {
                if let Some(l) = &last
                    && !zoom
                    && (f.width, f.height) != (l.width, l.height)
                {
                    stop_reason = Some(format!(
                        "the output changed size ({}x{} → {}x{}); recording stopped there",
                        l.width, l.height, f.width, f.height
                    ));
                    break;
                }
                last = Some(f);
                last_at = if paused { Instant::now() } else { at.max(last_at) };
                fresh = true;
            }
        }
        let Some(frame) = &last else {
            if started.elapsed() > Duration::from_secs(5) {
                bail!("no frames from the compositor after 5 s");
            }
            continue;
        };
        let animating = cam.as_ref().is_some_and(|c| c.animating(frame.width, frame.height));
        if paused || !(fresh || animating) || last_write.elapsed() < interval && enc.is_some() {
            continue;
        }
        // A new capture is stamped with its capture time; a frame that only
        // moves the zoom along, with now.
        let at = if fresh { last_at } else { Instant::now() };
        let rendered = if zoom {
            let c = cam.get_or_insert_with(|| camera::Camera::new(frame.width, frame.height));
            c.step(frame.width, frame.height, at);
            Some(c.render(frame))
        } else {
            None
        };
        let out = rendered.as_ref().unwrap_or(frame);
        let e = match &mut enc {
            Some(e) => e,
            None => {
                t0 = Some(at);
                enc.insert(encode::Encoder::spawn(codec, out.width, out.height, out.order, fps, path)?)
            }
        };
        let ts = stamp(at, t0.unwrap(), paused_total, &mut last_ts);
        e.write(&out.data, ts)?;
        written += 1;
        last_write = Instant::now();
        fresh = false;
    }

    let Some(mut e) = enc else { bail!("stopped before the first frame arrived") };
    let t0 = t0.unwrap();
    let stopped = if STOP.load(Ordering::SeqCst) { instant_at(STOPPED_AT.load(Ordering::SeqCst)) } else { Instant::now() };
    if let Some(since) = paused_since {
        paused_total += stopped.saturating_duration_since(since);
    }
    // Stamp the last picture again at the stop time so a still screen at the
    // end isn't cut off; it lasts one frame interval (the track's default).
    let end = stamp(stopped, t0, paused_total, &mut last_ts);
    if let Some(f) = &last {
        match &mut cam {
            Some(c) => {
                c.step(f.width, f.height, stopped);
                e.write(&c.render(f).data, end)?;
            }
            None => e.write(&f.data, end)?,
        }
    }
    e.finish()?;
    if let Some(r) = stop_reason {
        eprintln!("clipt: {r}");
    }
    let secs = end as f64 / 1000.0;
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        let c = captured.load(Ordering::Relaxed);
        let wall = started.elapsed().as_secs_f64();
        eprintln!("clipt: recorded {secs:.1} s of {wall:.1} s: {c} frames captured ({:.1}/s), {written} sent to ffmpeg", c as f64 / wall);
    }
    Ok(format!("{secs:.0} s, {}", codec.name()))
}

fn debug(msg: &str) {
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {msg}");
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let s = rustix::fs::statvfs(dir).ok()?;
    Some(s.f_bavail * s.f_frsize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, x: i32, w: u32) -> Output {
        Output { index: 0, name: name.into(), description: String::new(), x, y: 0, width: w, height: 720, native: (0, 0) }
    }

    #[test]
    fn geometry_must_fit_one_output() {
        let outs = [out("A", 0, 1280), out("B", 1280, 1920)];
        let (o, crop) = resolve_target(&outs, &Target::Geometry("1300,10 100x100".into())).unwrap();
        assert_eq!(o.name, "B");
        assert_eq!(crop, Some((20.0, 10.0, 100.0, 100.0)));
        assert!(resolve_target(&outs, &Target::Geometry("1200,10 200x100".into())).is_err(), "spans both");
        assert!(resolve_target(&outs, &Target::Screen(Some("C".into()))).is_err());
        assert_eq!(resolve_target(&outs, &Target::Screen(None)).unwrap().0.name, "A");
    }

    #[test]
    fn overlay_selection_is_trimmed_to_its_centre_output() {
        let outs = [out("A", 0, 1280), out("B", 1280, 1920)];
        // centre at x=1340, on B: the part on A is dropped
        let (o, crop) = resolve_target(&outs, &Target::Rect((1200.0, 10.0, 280.0, 100.0))).unwrap();
        assert_eq!(o.name, "B");
        assert_eq!(crop, Some((0.0, 10.0, 200.0, 100.0)));
    }

    #[test]
    fn extensions() {
        assert!(check_extension(Path::new("a.mp4")).is_ok());
        assert!(check_extension(Path::new("a.MKV")).is_ok());
        assert!(check_extension(Path::new("a.gif")).is_ok() && is_gif(Path::new("a.GIF")));
        assert!(check_extension(Path::new("a.webm")).is_err());
        assert_eq!(hidden_sibling(Path::new("/v/Rec_1.gif"), "recording.mp4"), Path::new("/v/.Rec_1.recording.mp4"));
        assert!(check_extension(Path::new("a")).is_err());
    }
}
