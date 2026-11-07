//! Sound for recordings: which sources to record, and the 20 ms chunks
//! each one's capture hands over. The sound server's side is in `pulse`
//! (Wayland and X11), `wasapi` (Windows) and `macos/sound.rs`.
//!
//! Chunks are stamped by a sample clock rather than by when each read
//! returns: the server hands audio over in its own blocks (PipeWire's are
//! 21.3 ms against our 20 ms reads), so read times jitter by ±15 ms while the
//! sample count doesn't. See docs/audio-plan.md for the measurements.

use anyhow::{Result, bail};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[cfg(wayland_x11)]
mod pulse;
#[cfg(wayland_x11)]
pub use pulse::{check, devices, open};
#[cfg(windows)]
mod wasapi;
#[cfg(target_os = "macos")]
pub use crate::macos::sound::{check, open};
#[cfg(windows)]
pub use wasapi::{check, devices, open};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 16-bit samples.
pub const BYTES_PER_FRAME: u32 = 2 * CHANNELS;
/// 20 ms.
pub(crate) const CHUNK_FRAMES: u32 = RATE / 50;

/// Most sources one recording takes (each is a Matroska track, 2 to 9).
pub const MAX_SOURCES: usize = 8;

/// Where one source's sound comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Device {
    /// What the computer is playing: the default output's monitor.
    System,
    /// The default input.
    Mic,
    /// A source by its server name, as `clipt list sound` shows it.
    Named(String),
}

/// One source and its volume in the mix.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub device: Device,
    /// 1.0 = as recorded.
    pub volume: f32,
}

impl Source {
    /// A `record_audio` value or `--audio=` argument: "none", "both", or
    /// sources separated by commas, each `system`, `mic` or a device name,
    /// optionally followed by `:PERCENT`.
    pub fn parse(s: &str) -> Result<Vec<Self>> {
        let s = s.trim();
        if s.is_empty() || s == "none" {
            return Ok(Vec::new());
        }
        let s = if s == "both" { "system,mic" } else { s };
        let mut v: Vec<Self> = Vec::new();
        for part in s.split(',').map(str::trim) {
            // Device names don't contain ':', but check the volume is a number
            // before splitting on it all the same.
            let (name, volume) = match part.rsplit_once(':') {
                Some((n, pct)) if pct.parse::<f32>().is_ok() => {
                    let pct: f32 = pct.parse().unwrap();
                    if !(0.0..=400.0).contains(&pct) {
                        bail!("volume {pct} for '{n}' is outside 0 to 400 %");
                    }
                    (n, pct / 100.0)
                }
                _ => (part, 1.0),
            };
            let device = match name {
                "" => bail!("empty audio source in '{s}'"),
                "none" | "both" => bail!("'{name}' can't be combined with other sources"),
                "system" => Device::System,
                "mic" => Device::Mic,
                n => Device::Named(n.into()),
            };
            if v.iter().any(|x| x.device == device) {
                bail!("'{name}' is listed twice");
            }
            v.push(Self { device, volume });
        }
        if v.len() > MAX_SOURCES {
            bail!("at most {MAX_SOURCES} audio sources");
        }
        Ok(v)
    }

    /// Whether `sources` include the computer's sound and the microphone, as
    /// the overlay's toggles show them.
    pub fn toggles(sources: &[Self]) -> (bool, bool) {
        (sources.iter().any(|s| s.device == Device::System), sources.iter().any(|s| s.device == Device::Mic))
    }

    /// `sources` with `system` and `mic` turned on or off as the overlay left
    /// them: a source turned off goes, one turned on is added at full volume,
    /// and everything else (named devices, volumes) stays as it was.
    pub fn with_toggles(mut sources: Vec<Self>, system: bool, mic: bool) -> Vec<Self> {
        for (device, on) in [(Device::System, system), (Device::Mic, mic)] {
            let have = sources.iter().any(|s| s.device == device);
            if have && !on {
                sources.retain(|s| s.device != device);
            } else if on && !have {
                sources.push(Self { device, volume: 1.0 });
            }
        }
        sources
    }

    /// The track's title in the file.
    pub fn label(&self) -> String {
        match &self.device {
            Device::System => "System sound".into(),
            Device::Mic => "Microphone".into(),
            Device::Named(n) => n.clone(),
        }
    }
}

/// 20 ms of interleaved 16-bit stereo, and when its first sample was heard.
pub struct Chunk {
    pub at: Instant,
    pub data: Vec<u8>,
}

/// Stamps chunks as an anchor plus samples read so far. Each read gives an
/// estimate of the anchor (when it returned, minus the latency and what had
/// been read); reads only ever return late, so the earliest estimate is the
/// best. Taking it over a sliding window lets it follow slow clock drift.
pub(crate) struct SampleClock {
    base: Instant,
    /// Frames read before the current chunk.
    frames: u64,
    /// (when, anchor estimate in seconds after `base`), anchors increasing:
    /// the front is the earliest within the window.
    window: VecDeque<(Instant, f64)>,
}

const WINDOW: Duration = Duration::from_secs(10);

impl SampleClock {
    pub(crate) fn new(base: Instant) -> Self {
        Self { base, frames: 0, window: VecDeque::new() }
    }

    /// When the first sample of a chunk of `frames` frames, whose read
    /// returned at `now` with `latency` still to go, was heard.
    pub(crate) fn stamp(&mut self, now: Instant, latency: Duration, frames: u64) -> Instant {
        let since_base =
            |t: Instant| t.saturating_duration_since(self.base).as_secs_f64() - self.base.saturating_duration_since(t).as_secs_f64();
        let end = since_base(now) - latency.as_secs_f64();
        let read = (self.frames + frames) as f64 / RATE as f64;
        let anchor = end - read;
        while self.window.back().is_some_and(|&(_, a)| a >= anchor) {
            self.window.pop_back();
        }
        self.window.push_back((now, anchor));
        while self.window.front().is_some_and(|&(t, _)| now.duration_since(t) > WINDOW) {
            self.window.pop_front();
        }
        let anchor = self.window.front().unwrap().1;
        let start = anchor + self.frames as f64 / RATE as f64;
        self.frames += frames;
        if start >= 0.0 {
            self.base + Duration::from_secs_f64(start)
        } else {
            self.base.checked_sub(Duration::from_secs_f64(-start)).unwrap_or(self.base)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        let s = |d: Device, volume: f32| Source { device: d, volume };
        assert!(Source::parse("none").unwrap().is_empty());
        assert!(Source::parse("").unwrap().is_empty());
        assert_eq!(Source::parse("system").unwrap(), [s(Device::System, 1.0)]);
        assert_eq!(Source::parse("both").unwrap(), [s(Device::System, 1.0), s(Device::Mic, 1.0)]);
        assert_eq!(Source::parse("system:70, mic:120").unwrap(), [s(Device::System, 0.7), s(Device::Mic, 1.2)]);
        assert_eq!(
            Source::parse("alsa_input.usb-Foo-00.analog-stereo:50").unwrap(),
            [s(Device::Named("alsa_input.usb-Foo-00.analog-stereo".into()), 0.5)]
        );
        assert_eq!(Source::parse("mic:0").unwrap(), [s(Device::Mic, 0.0)], "muted in the mix, still a track");
        assert!(Source::parse("system:500").is_err(), "too loud");
        assert!(Source::parse("system,system:50").is_err(), "twice");
        assert!(Source::parse("none,mic").is_err());
        assert!(Source::parse("system,").is_err());
        assert!(Source::parse(&vec!["mic"; 1].into_iter().chain((0..8).map(|_| "x")).collect::<Vec<_>>().join(",")).is_err());
    }

    #[test]
    fn overlay_toggles() {
        let v = Source::parse("system:70,alsa_input.x:50").unwrap();
        assert_eq!(Source::toggles(&v), (true, false));
        // Turning the mic on adds it at 100 %; system keeps its 70 %.
        assert_eq!(Source::with_toggles(v.clone(), true, true), Source::parse("system:70,alsa_input.x:50,mic").unwrap());
        // Turning system off leaves the named device alone.
        assert_eq!(Source::with_toggles(v.clone(), false, false), Source::parse("alsa_input.x:50").unwrap());
        assert_eq!(Source::with_toggles(Vec::new(), false, false), Vec::new());
    }

    /// Reads that return late by a varying amount (as PipeWire's blocks make
    /// them) still give evenly spaced stamps once the earliest one is seen.
    #[test]
    fn stamps_ignore_late_reads() {
        let base = Instant::now();
        let mut c = SampleClock::new(base);
        let chunk = Duration::from_millis(20);
        let lat = Duration::from_millis(5);
        // Sample 0 was heard at base + 100 ms; read k returns when its chunk
        // ends, plus the latency, plus up to 15 ms of lateness.
        let heard = base + Duration::from_millis(100);
        let mut stamps = Vec::new();
        for k in 0..20u32 {
            let late = Duration::from_millis([7, 0, 15, 3, 11][k as usize % 5]);
            let ret = heard + chunk * (k + 1) + lat + late;
            stamps.push(c.stamp(ret, lat, CHUNK_FRAMES as u64));
        }
        // From the second read on (the first on-time one), stamps are exact.
        for (k, s) in stamps.iter().enumerate().skip(1) {
            let want = heard + chunk * k as u32;
            let off = s.duration_since(want).as_secs_f64() - want.saturating_duration_since(*s).as_secs_f64();
            assert!(off.abs() < 0.0005, "chunk {k}: {:.1} ms off", off * 1000.0);
        }
    }

    /// The anchor follows a clock that drifts, rather than sticking to an
    /// estimate from long ago.
    #[test]
    fn window_follows_drift() {
        let base = Instant::now();
        let mut c = SampleClock::new(base);
        // The sound card runs 0.1 % slow: each 20 ms chunk takes 20.02 ms.
        let per = Duration::from_micros(20_020);
        let mut last = base;
        for k in 0..2000u32 {
            let ret = base + per * (k + 1);
            last = c.stamp(ret, Duration::ZERO, CHUNK_FRAMES as u64);
        }
        // Chunk 1999 started at 1999 × 20.02 ms; a fixed anchor would say
        // 1999 × 20 ms, 40 ms early. Within the 10 s window the error is
        // at most the drift over the window (10 ms).
        let want = base + per * 1999;
        let off = (last.duration_since(want).as_secs_f64() - want.saturating_duration_since(last).as_secs_f64()).abs();
        assert!(off < 0.011, "{:.1} ms off", off * 1000.0);
    }
}
