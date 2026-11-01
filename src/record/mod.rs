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
}

/// How this desktop lets us record.
enum Method {
    Wlroots(Protocol),
    #[cfg(feature = "pipewire")]
    Kde,
    #[cfg(feature = "pipewire")]
    Gnome,
}

pub struct Options {
    pub target: Target,
    pub output: Option<String>,
    pub fps: Option<u32>,
    pub cursor: bool,
    pub backend: Option<Backend>,
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
    let now = monotonic_ns();
    match sig {
        libc::SIGUSR1 => {
            if !PAUSED.swap(true, Ordering::SeqCst) {
                PAUSED_AT.store(now, Ordering::SeqCst);
            }
        }
        libc::SIGUSR2 => {
            if PAUSED.swap(false, Ordering::SeqCst) {
                RESUMED_AT.store(now, Ordering::SeqCst);
            }
        }
        _ => {
            if !STOP.swap(true, Ordering::SeqCst) {
                STOPPED_AT.store(now, Ordering::SeqCst);
            }
        }
    }
}

fn pidfile() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("clipt-record.pid")
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The running recording, if any: its pid and whether it's paused. The
/// pidfile holds the pid on the first line and "paused" or "recording" on
/// the second.
pub fn status() -> Option<(i32, bool)> {
    let s = std::fs::read_to_string(pidfile()).ok()?;
    let mut lines = s.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    alive(pid).then(|| (pid, lines.next() == Some("paused")))
}

/// Holds the pidfile for as long as the recording runs.
struct Lock(PathBuf);

impl Lock {
    fn take() -> Result<Self> {
        let p = pidfile();
        if let Some((pid, _)) = status() {
            bail!("already recording (pid {pid}); stop it with 'clipt record --stop'");
        }
        let _ = std::fs::remove_file(&p); // stale
        std::fs::OpenOptions::new().write(true).create_new(true).open(&p).with_context(|| format!("creating {}", p.display()))?;
        let lock = Self(p);
        lock.set(false)?;
        Ok(lock)
    }

    fn set(&self, paused: bool) -> Result<()> {
        let tmp = self.0.with_extension("tmp");
        std::fs::write(&tmp, format!("{}\n{}\n", std::process::id(), if paused { "paused" } else { "recording" }))?;
        std::fs::rename(&tmp, &self.0)?;
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Fail early, before the overlay opens, if a recording is already running.
pub fn ensure_idle() -> Result<()> {
    match status() {
        Some((pid, _)) => bail!("already recording (pid {pid}); stop it with 'clipt record --stop'"),
        None => Ok(()),
    }
}

/// `clipt record --stop`: signal the running recording and wait for it to
/// finish writing.
pub fn stop() -> Result<()> {
    let Some((pid, _)) = status() else { bail!("no recording is running") };
    unsafe { libc::kill(pid, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(30);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(!alive(pid), "recording (pid {pid}) didn't stop within 30 s");
    Ok(())
}

pub enum PauseCmd {
    Pause,
    Resume,
    Toggle,
}

/// `clipt record --pause / --resume / --toggle-pause`.
pub fn pause(cmd: PauseCmd) -> Result<()> {
    let Some((pid, paused)) = status() else { bail!("no recording is running") };
    let pause = match cmd {
        PauseCmd::Pause => true,
        PauseCmd::Resume => false,
        PauseCmd::Toggle => !paused,
    };
    unsafe { libc::kill(pid, if pause { libc::SIGUSR1 } else { libc::SIGUSR2 }) };
    Ok(())
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

    let mut wl = Wayland::connect().map_err(|e| e.context("recording needs a Wayland session for now (X11 is planned)"))?;
    let outputs = wl.outputs();
    anyhow::ensure!(!outputs.is_empty(), "no outputs found");
    let wlroots = wl.protocols();
    // `zoom`: frames change size, so they go through the camera.
    let method = pick_method(&wl, &wlroots, opts.backend)?;
    #[cfg(feature = "pipewire")]
    if matches!(method, Method::Kde | Method::Gnome) {
        crate::capture::pipewire::check()?;
    }
    let proto = match method {
        Method::Wlroots(p) => p,
        #[allow(unreachable_patterns)]
        _ => Protocol::ExtImageCopy,
    };
    let (source, zoom, follow, frame) = match method {
        Method::Wlroots(p) => wlroots_source(&wl, p, &outputs, &opts.target)?,
        #[cfg(feature = "pipewire")]
        Method::Kde => kde_source(&outputs, &opts.target)?,
        #[cfg(feature = "pipewire")]
        Method::Gnome => gnome_source(&outputs, &opts.target)?,
    };

    let path = match &opts.output {
        Some(p) => crate::config::expand(p),
        None => cfg.next_video_path(),
    };
    check_extension(&path)?;
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
    if let (Source::Output { crop, .. }, Some((id, out))) = (&source, &follow) {
        let (crop, id, out) = (crop.clone(), id.clone(), out.clone());
        std::thread::spawn(move || follow::run(&id, &out, &crop));
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
                Source::Output { out, crop } => wl.stream(out, cursor, proto, &|| *crop.lock().unwrap(), &mut sink),
                Source::Window { app, title } => wl.stream_window(app, title, cursor, &mut sink),
                #[cfg(feature = "pipewire")]
                Source::Kde(t) => crate::capture::kwin_cast::stream(t.clone(), cursor, Box::new(|_, _| None), Box::new(sink)),
                #[cfg(feature = "pipewire")]
                Source::Gnome(t) => crate::capture::mutter_cast::stream(t.clone(), cursor, Box::new(|_, _| None), Box::new(sink)),
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

    let result = write_loop(cfg, &lock, &slot, &codec, fps, zoom, &path, &captured);
    match &result {
        Ok(stats) => {
            println!("{}", path.display());
            if cfg.notify {
                crate::notify::send("Recording saved", &format!("{}  ·  {}", path.display(), stats), None);
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
    let (kde, gnome) = (
        || crate::capture::kwin_cast::available(wl.connection()),
        || crate::dbus::session().is_ok_and(|c| crate::capture::mutter_cast::available(&c)),
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
            anyhow::ensure!(gnome(), "Mutter's screencast isn't available here (the ScreenCast portal is planned)");
            Method::Gnome
        }
        Some(b) => bail!("recording doesn't support the {b:?} backend (ext, wlr, kwin or portal)"),
        None if ext => Method::Wlroots(Protocol::ExtImageCopy),
        None if !wlroots.is_empty() => Method::Wlroots(Protocol::WlrScreencopy),
        #[cfg(feature = "pipewire")]
        None if kde() => Method::Kde,
        #[cfg(feature = "pipewire")]
        None if gnome() => Method::Gnome,
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
        Some("mp4" | "mkv" | "mov") => Ok(()),
        _ => bail!("{}: recordings are .mp4, .mkv or .mov", path.display()),
    }
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
        assert!(check_extension(Path::new("a.gif")).is_err());
        assert!(check_extension(Path::new("a")).is_err());
    }
}
