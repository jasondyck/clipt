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

mod encode;

use crate::capture::wayland::{Protocol, Wayland};
use crate::capture::{Backend, LogicalRect, Output, VideoFrame};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub enum Target {
    /// One output, by name (default: the first).
    Screen(Option<String>),
    /// Slurp-style "X,Y WxH" in logical desktop coordinates.
    Geometry(String),
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

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn pidfile() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("clipt-record.pid")
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn running_pid() -> Option<i32> {
    let pid = std::fs::read_to_string(pidfile()).ok()?.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

/// Holds the pidfile for as long as the recording runs.
struct Lock(PathBuf);

impl Lock {
    fn take() -> Result<Self> {
        let p = pidfile();
        if let Some(pid) = running_pid() {
            bail!("already recording (pid {pid}); stop it with 'clipt record --stop'");
        }
        let _ = std::fs::remove_file(&p); // stale
        let mut f =
            std::fs::OpenOptions::new().write(true).create_new(true).open(&p).with_context(|| format!("creating {}", p.display()))?;
        use std::io::Write;
        writeln!(f, "{}", std::process::id())?;
        Ok(Self(p))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `clipt record --stop`: signal the running recording and wait for it to
/// finish writing.
pub fn stop() -> Result<()> {
    let Some(pid) = running_pid() else { bail!("no recording is running") };
    unsafe { libc::kill(pid, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(30);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::ensure!(!alive(pid), "recording (pid {pid}) didn't stop within 30 s");
    Ok(())
}

/// The newest captured frame, shared between the capture thread and the writer.
#[derive(Default)]
struct Slot {
    frame: Option<VideoFrame>,
    error: Option<String>,
    done: bool,
}

pub fn run(cfg: &Config, opts: Options) -> Result<()> {
    let fps = opts.fps.unwrap_or(cfg.record_fps).clamp(1, 240);
    let codec = encode::choose(&cfg.record_codec)?;

    let mut wl = Wayland::connect()
        .map_err(|e| e.context("recording works on wlroots-family Wayland compositors for now (X11, KDE and GNOME are planned)"))?;
    let proto = pick_protocol(&wl, opts.backend)?;
    let outputs = wl.outputs();
    anyhow::ensure!(!outputs.is_empty(), "no outputs found");
    let (out, crop) = resolve_target(&outputs, &opts.target)?;

    let path = match &opts.output {
        Some(p) => crate::config::expand(p),
        None => cfg.next_video_path(),
    };
    check_extension(&path)?;
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }

    let _lock = Lock::take()?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
    let captured = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let (slot, captured, cursor) = (slot.clone(), captured.clone(), opts.cursor);
        // The thread may still be blocked waiting for the screen to change
        // when we finish; process exit ends it.
        std::thread::spawn(move || {
            let mut sink = |f: VideoFrame| {
                captured.fetch_add(1, Ordering::Relaxed);
                let (m, cv) = &*slot;
                m.lock().unwrap().frame = Some(f);
                cv.notify_one();
                !STOP.load(Ordering::SeqCst)
            };
            let r = wl.stream(&out, cursor, proto, crop, &mut sink);
            let (m, cv) = &*slot;
            let mut s = m.lock().unwrap();
            s.done = true;
            if let Err(e) = r {
                s.error = Some(format!("{e:#}"));
            }
            cv.notify_one();
        });
    }

    let result = write_loop(cfg, &slot, &codec, fps, &path, &captured);
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

fn pick_protocol(wl: &Wayland, forced: Option<Backend>) -> Result<Protocol> {
    let have = wl.protocols();
    let want = match forced {
        None => {
            return have
                .first()
                .copied()
                .context("this compositor offers no screencopy protocol; recording on KDE and GNOME (PipeWire) is planned");
        }
        Some(Backend::Ext) => Protocol::ExtImageCopy,
        Some(Backend::Wlr) => Protocol::WlrScreencopy,
        Some(b) => bail!("recording doesn't support the {b:?} backend yet (ext or wlr)"),
    };
    anyhow::ensure!(have.contains(&want), "compositor doesn't offer {want:?}");
    Ok(want)
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
    }
}

fn check_extension(path: &Path) -> Result<()> {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mp4" | "mkv" | "mov") => Ok(()),
        _ => bail!("{}: recordings are .mp4, .mkv or .mov", path.display()),
    }
}

/// Feed frames to ffmpeg until stopped; returns a short summary.
fn write_loop(
    cfg: &Config,
    slot: &Arc<(Mutex<Slot>, Condvar)>,
    codec: &encode::Codec,
    fps: u32,
    path: &Path,
    captured: &std::sync::atomic::AtomicU64,
) -> Result<String> {
    let (m, cv) = &**slot;
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let started = Instant::now();
    let max = (cfg.max_duration > 0).then(|| Duration::from_secs(cfg.max_duration));
    let mut enc: Option<encode::Encoder> = None;
    let mut last: Option<VideoFrame> = None;
    let mut last_write = Instant::now() - interval;
    let mut written = 0u64;
    let mut last_disk_check = Instant::now();
    let mut stop_reason: Option<String> = None;

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
        // Wait for a new frame, but no sooner than the frame interval.
        let wait = interval.saturating_sub(last_write.elapsed());
        if !wait.is_zero() {
            std::thread::sleep(wait.min(Duration::from_millis(100)));
            continue;
        }
        let frame = {
            let mut s = m.lock().unwrap();
            if s.frame.is_none() && !s.done {
                s = cv.wait_timeout(s, Duration::from_millis(100)).unwrap().0;
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
            s.frame.take()
        };
        let Some(frame) = frame else {
            if enc.is_none() && started.elapsed() > Duration::from_secs(5) {
                bail!("no frames from the compositor after 5 s");
            }
            continue;
        };
        let e = match &mut enc {
            Some(e) => e,
            None => enc.insert(encode::Encoder::spawn(codec, frame.width, frame.height, frame.order, path)?),
        };
        if (frame.width, frame.height) != (e.width, e.height) {
            stop_reason = Some(format!(
                "the output changed size ({}x{} → {}x{}); recording stopped there",
                e.width, e.height, frame.width, frame.height
            ));
            break;
        }
        e.write(&frame.data)?;
        written += 1;
        last_write = Instant::now();
        last = Some(frame);
    }

    let Some(mut e) = enc else { bail!("stopped before the first frame arrived") };
    // Stamp the last picture again at the stop time so a still screen at the
    // end isn't cut off, and once more a frame later to give it a duration.
    if let Some(f) = &last
        && (f.width, f.height) == (e.width, e.height)
    {
        e.write(&f.data)?;
        std::thread::sleep(interval);
        e.write(&f.data)?;
    }
    e.finish()?;
    if let Some(r) = stop_reason {
        eprintln!("clipt: {r}");
    }
    let secs = started.elapsed().as_secs_f64();
    let summary = format!("{:.0} s, {}", secs, codec.name());
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        let c = captured.load(Ordering::Relaxed);
        eprintln!("clipt: recorded {secs:.1} s: {c} frames captured ({:.1}/s), {written} sent to ffmpeg", c as f64 / secs);
    }
    Ok(summary)
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
    fn extensions() {
        assert!(check_extension(Path::new("a.mp4")).is_ok());
        assert!(check_extension(Path::new("a.MKV")).is_ok());
        assert!(check_extension(Path::new("a.gif")).is_err());
        assert!(check_extension(Path::new("a")).is_err());
    }
}
