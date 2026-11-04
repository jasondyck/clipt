//! `clipt record`: screen, region and window recording.
//!
//! A capture thread streams frames from the compositor and keeps only the
//! newest one; the main thread hands frames to ffmpeg at most `fps` times a
//! second. A slow encoder therefore drops frames rather than queueing them.
//! Sound, when recorded, comes from its own thread in 20 ms chunks, which
//! are queued rather than dropped (a gap would be heard).
//!
//! One recording runs at a time, holding a lock on
//! `$XDG_RUNTIME_DIR/clipt-record.pid`; `clipt record --stop` (and Ctrl-C,
//! SIGTERM) end it cleanly, letting ffmpeg finish the file.
//!
//! See docs/recording-plan.md and docs/audio-plan.md.

pub mod audio;
mod camera;
mod clarity;
mod control;
mod encode;
#[cfg(wayland_x11)]
mod follow;
#[cfg(wayland_x11)]
mod indicator;
#[cfg(wayland_x11)]
mod linux;
mod mkv;

#[cfg(windows)]
use crate::win32::record as host;
#[cfg(wayland_x11)]
use linux as host;

pub use control::{PauseCmd, ensure_idle, pause, status, stop};
pub use encode::Balance;

use control::{CMD_STOP, Lock, PAUSED, PAUSED_AT, RESUMED_AT, STOP, STOPPED_AT, apply, free_bytes, instant_at};

#[cfg(wayland_x11)]
use crate::capture::Backend;
use crate::capture::{LogicalRect, Output, VideoFrame};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
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

pub struct Options {
    pub target: Target,
    pub output: Option<String>,
    pub fps: Option<u32>,
    pub cursor: bool,
    #[cfg(wayland_x11)]
    pub backend: Option<Backend>,
    /// GIF or not, over `record_format`: from `--gif` or the overlay's
    /// toggle. A `.gif` output name also makes a GIF.
    pub gif: Option<bool>,
    /// Sound to record alongside, mixed at each source's volume.
    pub audio: Vec<audio::Source>,
    /// Also keep each source as its own sound track.
    pub separate_audio: bool,
    /// Duck the other sources under the microphone, cleaning it up first.
    pub balance: Balance,
}

/// Sound queued for the writer beyond this means ffmpeg has stalled.
const MAX_QUEUED_AUDIO: usize = 250; // 5 s of 20 ms chunks

/// Stop recordings below this much free space, before ffmpeg hits ENOSPC
/// and leaves an unplayable file.
const MIN_FREE_BYTES: u64 = 500 << 20;

/// The newest captured frame, shared between the capture thread and the writer.
#[derive(Default)]
struct Slot {
    /// The frame and when it was captured.
    frame: Option<(VideoFrame, Instant)>,
    /// Sound chunks not yet handed to the writer, with their source's index,
    /// oldest first per source.
    audio: std::collections::VecDeque<(usize, audio::Chunk)>,
    /// How many sound threads have read everything up to the stop.
    audio_done: usize,
    error: Option<String>,
    done: bool,
}

/// Where frames go: false back from it stops the capture.
pub type Sink = Box<dyn FnMut(VideoFrame) -> bool + Send>;

/// A recording's frames, as the host (Wayland and X11, or Windows) sets
/// them up.
pub struct Feed {
    /// Captures frames into the sink until it says stop or capture fails.
    /// Runs on its own thread.
    pub stream: Box<dyn FnOnce(Sink) -> Result<()> + Send>,
    /// The frames change size (a window): they go through the camera.
    pub zoom: bool,
    /// What else runs once recording starts: the red frame around it,
    /// following a window.
    pub start: Box<dyn FnOnce()>,
}

pub fn run(cfg: &Config, opts: Options) -> Result<()> {
    let fps = opts.fps.unwrap_or(cfg.record_fps).clamp(1, 240);
    let codec = encode::choose(&cfg.record_codec)?;

    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let feed = host::feed(&opts, interval)?;
    let zoom = feed.zoom;

    let gif_format = match (opts.gif, cfg.record_format.as_str()) {
        (_, other) if !matches!(other, "mp4" | "gif") => bail!("unknown record_format '{other}' (mp4, gif)"),
        (Some(g), _) => g,
        (None, f) => f == "gif",
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
    let audio = if gif && !opts.audio.is_empty() {
        eprintln!("clipt: GIFs have no sound; recording without it");
        Vec::new()
    } else {
        opts.audio
    };
    // Opened before recording starts, so a missing sound server or device
    // stops it here.
    if !audio.is_empty() {
        encode::check_aac()?;
    }
    let captures = audio.iter().map(audio::open).collect::<Result<Vec<_>>>()?;
    let sound = (!audio.is_empty()).then(|| encode::Sound {
        volumes: audio.iter().map(|a| a.volume).collect(),
        labels: audio.iter().map(audio::Source::label).collect(),
        separate: opts.separate_audio,
        voice: audio.iter().position(|a| a.device == audio::Device::Mic),
        balance: opts.balance,
    });

    let lock = Lock::take()?;
    control::handle_signals();
    (feed.start)();

    let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
    let captured = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let (slot, captured, stream) = (slot.clone(), captured.clone(), feed.stream);
        // The thread may still be blocked waiting for the screen to change
        // when we finish; process exit ends it.
        std::thread::spawn(move || {
            let frames = slot.clone();
            let sink = Box::new(move |f: VideoFrame| {
                captured.fetch_add(1, Ordering::Relaxed);
                let (m, cv) = &*frames;
                m.lock().unwrap().frame = Some((f, Instant::now()));
                cv.notify_one();
                !STOP.load(Ordering::SeqCst)
            });
            let r = stream(sink);
            let (m, cv) = &*slot;
            let mut s = m.lock().unwrap();
            s.done = true;
            if let Err(e) = r {
                s.error = Some(format!("{e:#}"));
            }
            cv.notify_one();
        });
    }

    let sources = captures.len();
    for (i, cap) in captures.into_iter().enumerate() {
        let slot = slot.clone();
        std::thread::spawn(move || {
            let r = cap.run(|c| {
                let (m, cv) = &*slot;
                let mut s = m.lock().unwrap();
                if s.audio.len() >= MAX_QUEUED_AUDIO * sources {
                    s.error = Some("ffmpeg isn't keeping up with the sound".into());
                    cv.notify_one();
                    return false;
                }
                // After a stop, keep reading until the sound heard before it
                // is in: up to the server's latency still sits in its buffer.
                let more = !STOP.load(Ordering::SeqCst) || {
                    let stopped = instant_at(STOPPED_AT.load(Ordering::SeqCst));
                    c.at < stopped && Instant::now() < stopped + Duration::from_secs(1)
                };
                s.audio.push_back((i, c));
                cv.notify_one();
                more
            });
            let (m, cv) = &*slot;
            let mut s = m.lock().unwrap();
            s.audio_done += 1;
            if let Err(e) = r {
                s.error = Some(format!("{e:#}"));
            }
            cv.notify_one();
        });
    }

    let written = write_loop(cfg, &lock, &slot, &codec, fps, zoom, sound.as_ref(), &path, &captured);
    // `smart` kept the sound as recorded beside the video for the re-mix,
    // which is over now, however the recording ended.
    if sound.as_ref().is_some_and(encode::Sound::analyses) {
        let _ = std::fs::remove_file(encode::kept_path(&path));
    }
    let result = match (written, gif) {
        (Ok((_, duration)), true) => encode::to_gif(&path, &target, fps.min(15), duration).map(|()| {
            let _ = std::fs::remove_file(&path);
            "GIF".to_string()
        }),
        (Ok((stats, _)), false) => Ok(stats),
        (Err(e), _) => Err(e),
    };
    match &result {
        Ok(stats) => {
            println!("{}", target.display());
            if cfg.notify {
                #[cfg(wayland_x11)]
                crate::notify::send("Recording saved", &format!("{}  ·  {}", target.display(), stats), None);
                #[cfg(windows)]
                crate::win32::notify("Recording saved", &format!("{}  ·  {}", target.display(), stats), Some(&target));
            }
        }
        Err(_) if path.exists() => eprintln!("clipt: the partial recording is in {}", path.display()),
        Err(_) => {}
    }
    result.map(|_| ())
}

/// The output to capture and the crop within it (output-logical coordinates).
pub fn resolve_target(outputs: &[Output], target: &Target) -> Result<(Output, Option<LogicalRect>)> {
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
    sound: Option<&encode::Sound>,
    path: &Path,
    captured: &std::sync::atomic::AtomicU64,
) -> Result<(String, Duration)> {
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
    // Every pause, (from, to); sound is placed against these exactly.
    let mut pauses = Pauses::default();
    // Sound waiting for ffmpeg to start (at the first frame).
    let mut pending: std::collections::VecDeque<(usize, audio::Chunk)> = Default::default();
    let sources = sound.map_or(0, |s| s.volumes.len());
    // Each source's last timestamp.
    let mut last_audio_ts: Vec<Option<u64>> = vec![None; sources];
    // Which sources have delivered anything yet.
    let mut heard = vec![false; sources];
    let mut chunks = 0u64;
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
                let since = instant_at(PAUSED_AT.load(Ordering::SeqCst));
                paused_since = Some(since);
                pauses.start(since);
                let _ = lock.set(true);
            }
            (false, Some(since)) => {
                let resumed = instant_at(RESUMED_AT.load(Ordering::SeqCst)).max(since);
                paused_total += resumed - since;
                paused_since = None;
                pauses.end(resumed);
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
            if s.frame.is_none() && s.audio.is_empty() && !s.done && !wait.is_zero() {
                s = cv.wait_timeout(s, wait).unwrap().0;
            }
            for (i, _) in &s.audio {
                heard[*i] = true;
            }
            pending.extend(s.audio.drain(..));
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
        if started.elapsed() > Duration::from_secs(5)
            && let Some(i) = heard.iter().position(|h| !h)
        {
            let label = sound.map(|s| s.labels[i].clone()).unwrap_or_default();
            if let Some(e) = enc.take() {
                let _ = e.finish();
            }
            bail!("no sound from {label} after 5 s");
        }
        if let (Some(e), Some(t0)) = (&mut enc, t0) {
            chunks += send_audio(e, &mut pending, t0, &pauses, None, &mut last_audio_ts)?;
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
        // Pace from when this frame is due, not from when its write ends:
        // rendering and the pipe take a few milliseconds at 4K, and adding
        // them to every interval turned 30 fps into about 27.
        let due = Instant::now();
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
                enc.insert(encode::Encoder::spawn(codec, out.width, out.height, out.order, fps, sound, path)?)
            }
        };
        let ts = stamp(at, t0.unwrap(), paused_total, &mut last_ts);
        e.write(&out.data, ts)?;
        written += 1;
        last_write = due;
        fresh = false;
    }

    // Stopped here (time limit, disk space, …) rather than by a command: stop
    // the capture threads the same way.
    apply(CMD_STOP);
    let stopped = instant_at(STOPPED_AT.load(Ordering::SeqCst));
    let Some(mut e) = enc else { bail!("stopped before the first frame arrived") };
    let t0 = t0.unwrap();
    if let Some(since) = paused_since {
        paused_total += stopped.saturating_duration_since(since);
    }
    // The sound heard up to the stop, once the sound thread has read it.
    if sources > 0 {
        let s = m.lock().unwrap();
        let (mut s, _) = cv.wait_timeout_while(s, Duration::from_secs(2), |s| s.audio_done < sources).unwrap();
        pending.extend(s.audio.drain(..));
    }
    chunks += send_audio(&mut e, &mut pending, t0, &pauses, Some(stopped), &mut last_audio_ts)?;
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
    let clarity = e.finish()?;
    if let Some(r) = stop_reason {
        eprintln!("clipt: {r}");
    }
    if let Some(r) = &clarity {
        eprintln!("clipt: {r}");
    }
    let mut summary = clarity.map(|r| format!(", {}", r.summary())).unwrap_or_default();
    if let (Some(s), Some(clarity::Report::Measured { clarity: before, quieter: Some((d, _)), leak, .. })) = (sound, clarity)
        && leak != clarity::Leak::Heard
    {
        eprintln!("clipt: re-mixing with the other sound {d:.1} dB quieter");
        match encode::remix(s, path, d, Duration::from_millis(end)) {
            Ok(after) => {
                if let Some(after) = after {
                    eprintln!("clipt: re-mixed: {after}");
                }
                if let Some(clarity::Report::Measured { clarity: after, .. }) = after {
                    summary = format!(", speech clarity {before:.2}, {after:.2} re-mixed with the other sound {d:.1} dB quieter");
                }
            }
            Err(e) => eprintln!("clipt: {e:#}; the recording is as it was"),
        }
    }
    let secs = end as f64 / 1000.0;
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        let c = captured.load(Ordering::Relaxed);
        let wall = started.elapsed().as_secs_f64();
        eprintln!("clipt: recorded {secs:.1} s of {wall:.1} s: {c} frames captured ({:.1}/s), {written} sent to ffmpeg", c as f64 / wall);
        if sources > 0 {
            eprintln!("clipt: {chunks} sound chunks sent from {sources} source(s) ({:.1} s each)", chunks as f64 * 0.02 / sources as f64);
        }
    }
    Ok((format!("{secs:.0} s, {}{}{summary}", codec.name(), if sources > 0 { ", AAC" } else { "" }), Duration::from_millis(end)))
}

/// The pauses so far, in order; the last may still be running.
#[derive(Default)]
struct Pauses(Vec<(Instant, Option<Instant>)>);

impl Pauses {
    fn start(&mut self, at: Instant) {
        self.0.push((at, None));
    }

    fn end(&mut self, at: Instant) {
        if let Some(p) = self.0.last_mut() {
            p.1 = Some(at);
        }
    }

    /// Whether `at` falls inside a pause.
    fn contains(&self, at: Instant) -> bool {
        self.0.iter().any(|&(from, to)| at >= from && to.is_none_or(|to| at < to))
    }

    /// Paused time before `at`.
    fn before(&self, at: Instant) -> Duration {
        self.0.iter().filter_map(|&(from, to)| to.filter(|&to| to <= at).map(|to| to - from)).sum()
    }
}

/// Hand ffmpeg the queued sound: everything heard after `t0`, outside the
/// pauses and before `until`, stamped like frames (paused time left out).
/// Returns how many chunks were sent.
fn send_audio(
    e: &mut encode::Encoder,
    pending: &mut std::collections::VecDeque<(usize, audio::Chunk)>,
    t0: Instant,
    pauses: &Pauses,
    until: Option<Instant>,
    last_ts: &mut [Option<u64>],
) -> Result<u64> {
    let mut sent = 0;
    while let Some((i, c)) = pending.pop_front() {
        if c.at < t0 || until.is_some_and(|u| c.at >= u) || pauses.contains(c.at) {
            continue;
        }
        let ms = audio_ms(c.at, t0, pauses);
        // Strictly increasing; the resampler absorbs the overlap.
        let ms = last_ts[i].map_or(ms, |prev| ms.max(prev + 1));
        last_ts[i] = Some(ms);
        e.write_audio(i, &c.data, ms)?;
        sent += 1;
    }
    Ok(sent)
}

/// Milliseconds into the recording for sound heard at `at`.
fn audio_ms(at: Instant, t0: Instant, pauses: &Pauses) -> u64 {
    at.saturating_duration_since(t0).saturating_sub(pauses.before(at)).as_millis() as u64
}

fn debug(msg: &str) {
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {msg}");
    }
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
    fn sound_skips_paused_time() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut p = Pauses::default();
        p.start(ms(1000));
        assert!(p.contains(ms(1500)), "an open pause covers everything after it");
        p.end(ms(3000));
        assert!(!p.contains(ms(999)) && p.contains(ms(1000)) && p.contains(ms(2999)) && !p.contains(ms(3000)));
        // Heard just before the pause: as is. Just after: two seconds earlier.
        assert_eq!(audio_ms(ms(980), t0, &p), 980);
        assert_eq!(audio_ms(ms(3000), t0, &p), 1000);
        assert_eq!(audio_ms(ms(4500), t0, &p), 2500);
        p.start(ms(5000));
        p.end(ms(5500));
        assert_eq!(audio_ms(ms(6000), t0, &p), 3500);
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
