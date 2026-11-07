//! Video encoding through an `ffmpeg` subprocess fed raw frames on stdin.
//!
//! ffmpeg is optional: only recording needs it. The codec is picked once per
//! recording by test-encoding a frame with each candidate, so the GPU encoder
//! is used wherever one works and libx264 everywhere else:
//!   1. h264_vaapi (AMD, Intel), BGRA → NV12 on the GPU
//!   2. h264_nvenc (NVIDIA's driver has no VA-API encoding)
//!   3. libx264 on the CPU
//!
//! On Windows the GPU encoders are NVIDIA's, Intel's (Quick Sync) and AMD's
//! (AMF), then whatever Media Foundation offers, then libx264.
//!
//! Frames reach ffmpeg in a minimal Matroska stream (see `mkv.rs`) that
//! carries each frame's capture time, and the output keeps those times
//! (variable frame rate): frames only need sending when the screen changed,
//! and a slow encoder drops frames instead of slowing the video down. Sound,
//! when recorded, travels in the same stream with timestamps from the same
//! clock, and is encoded as AAC.

use crate::capture::PixelOrder;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// How long `Encoder::finish` waits for ffmpeg to exit after its input
/// closes: encoders flush their buffer in well under a second.
#[cfg(not(test))]
const FINISH_LIMIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const FINISH_LIMIT: Duration = Duration::from_millis(500);

/// How long `to_gif` may take: it reads the whole recording, so its limit
/// scales with the recording (60 s plus twice its length).
#[cfg(not(test))]
fn gif_limit(duration: Duration) -> Duration {
    Duration::from_secs(60) + duration.saturating_mul(2)
}
#[cfg(test)]
fn gif_limit(_duration: Duration) -> Duration {
    Duration::from_millis(500)
}

/// Wait for `child` to exit, at most `limit`; `None` means it's still running.
fn wait_up_to(child: &mut Child, limit: Duration) -> Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().context("waiting for ffmpeg")? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// SIGKILL and reap: a hung ffmpeg has already ignored SIGTERM.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    /// VA-API through this render node.
    Vaapi(String),
    Nvenc,
    /// Intel Quick Sync (Windows).
    Qsv,
    /// AMD (Windows).
    Amf,
    /// A Media Foundation hardware encoder (Windows).
    Mf,
    X264,
}

/// ffmpeg: on Windows, preferably the one beside clipt.exe (where the
/// README suggests putting it), and without a console window of its own,
/// which would flash up for every recording started from the tray. A
/// console-less ffmpeg also doesn't see Ctrl-C, which clipt handles.
pub fn ffmpeg() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let beside = std::env::current_exe().unwrap_or_default().with_file_name("ffmpeg.exe");
        let mut cmd = Command::new(if beside.is_file() { beside } else { "ffmpeg".into() });
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    Command::new("ffmpeg")
}

impl Codec {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Vaapi(_) => "h264_vaapi",
            Self::Nvenc => "h264_nvenc",
            Self::Qsv => "h264_qsv",
            Self::Amf => "h264_amf",
            Self::Mf => "h264_mf",
            Self::X264 => "libx264",
        }
    }

    /// ffmpeg arguments placed before the input.
    fn global_args(&self) -> Vec<String> {
        match self {
            Self::Vaapi(dev) => vec!["-vaapi_device".into(), dev.clone()],
            _ => vec![],
        }
    }

    /// Filters and encoder settings for a `w` x `h` input. H.264 needs even
    /// dimensions, so odd sizes get a one-pixel pad.
    fn output_args(&self, w: u32, h: u32) -> Vec<String> {
        let pad = "pad=ceil(iw/2)*2:ceil(ih/2)*2";
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match self {
            // No B-frames on any encoder: with variable frame rate and sparse
            // frames (a still screen) their reordered timestamps gave MP4
            // files a wrong duration (h264_vaapi on KDE, libx264 on X11).
            Self::Vaapi(_) => {
                v(&["-vf", &format!("{pad},hwupload,scale_vaapi=format=nv12"), "-c:v", "h264_vaapi", "-qp", "24", "-bf", "0"])
            }
            Self::Nvenc => v(&["-vf", pad, "-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "24", "-b:v", "0", "-bf", "0"]),
            Self::Qsv => v(&["-vf", &format!("{pad},format=nv12"), "-c:v", "h264_qsv", "-global_quality", "24", "-bf", "0"]),
            Self::Amf => {
                v(&["-vf", &format!("{pad},format=nv12"), "-c:v", "h264_amf", "-rc", "cqp", "-qp_i", "24", "-qp_p", "24", "-bf", "0"])
            }
            // hw_encoding: Media Foundation's own software encoder is no
            // better than libx264, which comes next.
            Self::Mf => v(&[
                "-vf",
                &format!("{pad},format=nv12"),
                "-c:v",
                "h264_mf",
                "-hw_encoding",
                "1",
                "-rate_control",
                "quality",
                "-quality",
                "70",
            ]),
            Self::X264 => {
                // Above 1440p, veryfast needs several cores to keep up.
                let preset = if w as u64 * h as u64 > 2560 * 1440 { "ultrafast" } else { "veryfast" };
                v(&["-vf", &format!("{pad},format=yuv420p"), "-c:v", "libx264", "-preset", preset, "-crf", "23", "-bf", "0"])
            }
        }
    }
}

/// Candidates for a `record_codec` setting, in the order they're tried.
fn candidates(pref: &str) -> Result<Vec<Codec>> {
    if cfg!(windows) {
        return Ok(match pref {
            "auto" => vec![Codec::Nvenc, Codec::Qsv, Codec::Amf, Codec::Mf, Codec::X264],
            "nvenc" => vec![Codec::Nvenc],
            "qsv" => vec![Codec::Qsv],
            "amf" => vec![Codec::Amf],
            "mf" => vec![Codec::Mf],
            "x264" => vec![Codec::X264],
            other => bail!("unknown record_codec '{other}' (auto, nvenc, qsv, amf, mf, x264)"),
        });
    }
    let vaapi = || render_nodes().into_iter().map(Codec::Vaapi).collect::<Vec<_>>();
    Ok(match pref {
        "auto" => {
            let mut v = vaapi();
            v.extend([Codec::Nvenc, Codec::X264]);
            v
        }
        "vaapi" => vaapi(),
        "nvenc" => vec![Codec::Nvenc],
        "x264" => vec![Codec::X264],
        other => bail!("unknown record_codec '{other}' (auto, vaapi, nvenc, x264)"),
    })
}

fn render_nodes() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .map(|n| format!("/dev/dri/{n}"))
        .collect();
    v.sort();
    v
}

/// The first codec that encodes a test frame, or an error naming what's missing.
pub fn choose(pref: &str) -> Result<Codec> {
    match ffmpeg().arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && cfg!(windows) => {
            bail!("recording needs ffmpeg, which isn't installed (winget install Gyan.FFmpeg, or put ffmpeg.exe beside clipt.exe)")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("recording needs ffmpeg, which isn't installed (install the 'ffmpeg' package)")
        }
        Err(e) => return Err(e).context("running ffmpeg"),
    }
    let mut tried = Vec::new();
    for c in candidates(pref)? {
        match probe(&c) {
            Ok(()) => {
                debug(&format!("encoder: {}", c.name()));
                return Ok(c);
            }
            Err(e) => {
                debug(&format!("encoder {} unusable: {e}", c.name()));
                tried.push(format!("{}: {e}", c.name()));
            }
        }
    }
    bail!("no working video encoder:\n  {}", tried.join("\n  "))
}

/// Encode one small frame the way a recording would, to see if this codec works here.
fn probe(c: &Codec) -> Result<()> {
    let (w, h) = (320, 240);
    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error"]).args(c.global_args());
    cmd.args(["-f", "lavfi", "-i", &format!("color=c=gray:s={w}x{h}:d=0.1,format=bgr0"), "-frames:v", "1"]);
    cmd.args(c.output_args(w, h)).args(["-f", "null", "-"]);
    let out = cmd.stdin(Stdio::null()).output().context("running ffmpeg")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    bail!("{}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim())
}

/// Fails, saying so, if this ffmpeg can't encode the AAC sound recordings
/// use or mix sources (a minimal build may leave them out).
pub fn check_aac() -> Result<()> {
    // Silence as 16-bit samples, as recordings send them: a minimal ffmpeg
    // (the Flatpak's) has that decoder, not the 8-bit one anullsrc gives.
    let silence = "anullsrc=r=48000:cl=stereo,aformat=sample_fmts=s16";
    let out = ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", silence, "-f", "lavfi", "-i", silence])
        .args([
            "-t",
            "0.1",
            "-filter_complex",
            &format!("[0:a]{RESAMPLE},volume=0.5[a];[1:a]{RESAMPLE}[b];[a][b]{MIX}:inputs=2[m]"),
            "-map",
            "[m]",
        ])
        .args(AAC)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .context("running ffmpeg")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("this ffmpeg can't encode sound (AAC): {}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim());
    }
    Ok(())
}

/// Each source first goes through the resampler: its async mode closes the
/// tiny gaps and overlaps between chunks stamped on the system clock, and
/// takes up the sound card's drift against it; `first_pts=0` fills any wait
/// before the first chunk with silence (PulseAudio takes over a second to
/// deliver it), so the sound starts with the video.
const RESAMPLE: &str = "aresample=async=1:first_pts=0";
/// Mixing at full volume each (`normalize=0`): the volumes given are the
/// volumes heard, rather than each source being divided by their number.
const MIX: &str = "amix=duration=longest:normalize=0";
const AAC: [&str; 4] = ["-c:a", "aac", "-b:a", "160k"];

/// What `record_audio_balance` / `--audio-balance` asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Balance {
    #[default]
    Off,
    /// Clean up the microphone's sound and duck the other sources under it.
    Duck,
    /// `Duck`, and measure how clearly the voice comes through (`clarity`).
    Smart,
}

impl Balance {
    /// A `record_audio_balance` value or `--audio-balance` argument.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "off" => Ok(Self::Off),
            "duck" => Ok(Self::Duck),
            "smart" => Ok(Self::Smart),
            other => bail!("unknown audio balance '{other}' (off, duck, smart)"),
        }
    }
}

/// Voice clean-up for `duck`, from the phase 4 spike: high-pass at 80 Hz,
/// 3:1 compression from -30 dB (threshold 0.0316), de-essing and a 3 kHz
/// presence boost. No make-up gain: `LEVEL` and `MATCH` restore the level.
const CLEAN_MIC: &str = "highpass=f=80,acompressor=threshold=0.0316:ratio=3,deesser,equalizer=f=3000:width_type=q:w=1:g=3";
/// The clean-up must not change how loud the voice is (a quieter voice
/// undoes the ducking; a louder one lifts its noise), and a compressor's
/// fixed make-up only matches at one level: 9.5 dB was right at -23 dBFS
/// but added 10 dB at -45. So the make-up follows the voice: the raw and
/// cleaned voice's power (L² + R², from an `amerge` of the two), each
/// smoothed over about 1.6 s, and the cleaned voice scaled by the square
/// root of their ratio, held within ±12 dB. Loud moments dominate both
/// sums, so the level is right from the first word, even after a pause.
/// `aeval` leaves the channel layout unset, and `lowpass` then filters
/// nothing, hence the `aformat`s.
const LEVEL: &str =
    "aeval='(val(0)*val(0)+val(1)*val(1))|(val(2)*val(2)+val(3)*val(3))':c=stereo,aformat=channel_layouts=stereo,lowpass=f=0.1:p=1";
const MATCH: &str = "aeval='val(0)*clip(sqrt((val(2)+1e-30)/(val(3)+1e-30)),0.25,4)|val(1)*clip(sqrt((val(2)+1e-30)/(val(3)+1e-30)),0.25,4)':c=stereo,aformat=channel_layouts=stereo";

/// `amerge` needs its inputs in one sample format: clipt sends 16-bit, and
/// the compressor gives doubles.
const FLOAT: &str = "aformat=sample_fmts=fltp:channel_layouts=stereo";

/// The voice from `[wet{i}]` cleaned up at its own level, split into `outs`.
fn clean_voice(i: usize, outs: &str) -> String {
    format!(
        "[wet{i}]{FLOAT},asplit=2[raw{i}][pre{i}];[pre{i}]{CLEAN_MIC},{FLOAT},asplit=2[cl{i}][cp{i}];\
         [raw{i}][cp{i}]amerge=inputs=2,{LEVEL}[lv{i}];[cl{i}][lv{i}]amerge=inputs=2,{MATCH},asplit=2{outs}"
    )
}
/// The ducking from the spike: about 11 dB of dip while the voice speaks.
const DUCK: &str = "sidechaincompress=threshold=0.02:ratio=8:attack=20:release=300";

/// The sound tracks of a recording: one per source, mixed at these volumes.
pub struct Sound {
    pub volumes: Vec<f32>,
    pub labels: Vec<String>,
    /// Also keep each source as its own track, at the volume it was recorded.
    pub separate: bool,
    /// The microphone's source index, when it is recorded: the voice `duck`
    /// works on. A named microphone device isn't the voice.
    pub voice: Option<usize>,
    pub balance: Balance,
}

impl Sound {
    /// The voice to duck under: `duck` and `smart` need the microphone and
    /// something else.
    fn ducking(&self) -> Option<usize> {
        let n = self.volumes.len();
        (self.balance != Balance::Off && n > 1).then_some(self.voice).flatten().filter(|&v| v < n)
    }

    /// Whether ffmpeg also sends back the mix's inputs for `clarity`.
    pub fn analyses(&self) -> bool {
        self.balance == Balance::Smart && self.ducking().is_some()
    }

    /// The output to clipt's pipe: the voice and each other source as they
    /// go into the mix, mono, 48 kHz floats, voice first.
    fn analysis_args(&self) -> Vec<String> {
        if !self.analyses() {
            return Vec::new();
        }
        ["-map", "[ana]", "-c:a", "pcm_f32le", "-f", "f32le", "pipe:1"].map(String::from).to_vec()
    }

    /// The output that keeps each source as recorded, losslessly, for the
    /// re-mix: `[t{i}]`, or `[r{i}]` when separate tracks also want them.
    fn kept_args(&self, kept: &Path) -> Vec<std::ffi::OsString> {
        if !self.analyses() {
            return Vec::new();
        }
        let n = self.volumes.len();
        let label = if self.separate && n > 1 { "r" } else { "t" };
        let mut a: Vec<std::ffi::OsString> = Vec::new();
        for i in 0..n {
            a.extend(["-map".into(), format!("[{label}{i}]").into()]);
        }
        a.extend(["-c:a", "flac", "-f", "matroska"].map(Into::into));
        a.push(kept.into());
        a
    }

    /// The same sound with every source but the voice `down` dB quieter.
    fn quieter(&self, down: f32) -> Sound {
        let voice = self.ducking();
        let g = 10f32.powf(-down / 20.0);
        Sound {
            volumes: self.volumes.iter().enumerate().map(|(i, &v)| if Some(i) == voice { v } else { v * g }).collect(),
            labels: self.labels.clone(),
            separate: self.separate,
            voice: self.voice,
            balance: self.balance,
        }
    }

    /// ffmpeg arguments: the filter graph, the outputs to map after the video,
    /// and their names. MP4 and MOV keep a track's name as its handler name
    /// (what players show); Matroska as its title.
    fn args(&self, mov: bool) -> Vec<String> {
        let duck = self.ducking();
        if self.balance != Balance::Off && duck.is_none() {
            eprintln!("clipt: audio balance: no microphone to duck under");
        }
        self.graph_args(0, mov, self.analyses())
    }

    /// The arguments for sound read from input `input`; `keep` also sends
    /// each source as recorded to `[r{i}]`, for the sound file `smart`
    /// re-mixes from.
    fn graph_args(&self, input: usize, mov: bool, keep: bool) -> Vec<String> {
        let name = if mov { "handler_name" } else { "title" };
        let n = self.volumes.len();
        let separate = self.separate && n > 1;
        let duck = self.ducking();
        // Each source as recorded, `[t{i}]`, for separate tracks, the kept
        // sound, or both.
        let raw = separate || keep;
        let mut graph = Vec::new();
        if let Some(voice) = duck {
            self.duck_graph(input, voice, raw, &mut graph);
        } else {
            for (i, &v) in self.volumes.iter().enumerate() {
                graph.push(if raw {
                    format!("[{input}:a:{i}]{RESAMPLE},asplit=2[m{i}][t{i}];[m{i}]volume={v}[v{i}]")
                } else {
                    format!("[{input}:a:{i}]{RESAMPLE}{}[v{i}]", volume_arg(v))
                });
            }
        }
        // With both, the sources as recorded are split again.
        let track = if separate && keep {
            for i in 0..n {
                graph.push(format!("[t{i}]asplit=2[s{i}][r{i}]"));
            }
            "s"
        } else {
            "t"
        };
        // For `smart`, each input to the mix is also sent back to clipt.
        let into_mix = if let Some(voice) = duck.filter(|_| self.analyses()) {
            let order = std::iter::once(voice).chain((0..n).filter(|&i| i != voice));
            let mut merge = String::new();
            for (k, i) in order.enumerate() {
                graph.push(format!("[v{i}]asplit=2[u{i}][y{i}];[y{i}]pan=mono|c0=0.5*c0+0.5*c1[z{k}]"));
                merge.push_str(&format!("[z{k}]"));
            }
            graph.push(format!("{merge}amerge=inputs={n},aformat=sample_fmts=flt[ana]"));
            "u"
        } else {
            "v"
        };
        let mix = if n == 1 {
            "v0".to_string()
        } else {
            let ins: String = (0..n).map(|i| format!("[{into_mix}{i}]")).collect();
            graph.push(format!("{ins}{MIX}:inputs={n}[mix]"));
            "mix".to_string()
        };
        let mut a = vec!["-filter_complex".to_string(), graph.join(";"), "-map".into(), "0:v".into(), "-map".into(), format!("[{mix}]")];
        let first = if n > 1 { "Mix" } else { &self.labels[0] };
        a.extend(["-metadata:s:a:0".into(), format!("{name}={first}")]);
        if separate {
            for (i, label) in self.labels.iter().enumerate() {
                a.extend(["-map".into(), format!("[{track}{i}]"), format!("-metadata:s:a:{}", i + 1), format!("{name}={label}")]);
            }
        }
        a.extend(AAC.iter().map(|s| s.to_string()));
        a
    }

    /// The `duck` graph: the microphone through the clean-up chain, feeding
    /// both the mix and the ducking keys; every other source, after its
    /// volume, is sidechained to the voice.
    fn duck_graph(&self, input: usize, voice: usize, separate: bool, graph: &mut Vec<String>) {
        let others: Vec<usize> = (0..self.volumes.len()).filter(|&i| i != voice).collect();
        // The clean voice is split into the mix and the keys. One key is
        // named directly; several need a second split.
        let key = if others.len() == 1 { format!("[k{}]", others[0]) } else { "[key]".to_string() };
        graph.push(if separate {
            format!("[{input}:a:{voice}]{RESAMPLE},asplit=2[wet{voice}][t{voice}]")
        } else {
            format!("[{input}:a:{voice}]{RESAMPLE}[wet{voice}]")
        });
        graph.push(clean_voice(voice, &format!("[m{voice}]{key}")));
        graph.push(format!("[m{voice}]anull{}[v{voice}]", volume_arg(self.volumes[voice])));
        if others.len() > 1 {
            let keys: String = others.iter().map(|i| format!("[k{i}]")).collect();
            graph.push(format!("[key]asplit={}{keys}", others.len()));
        }
        for &i in &others {
            let v = self.volumes[i];
            graph.push(if separate {
                format!("[{input}:a:{i}]{RESAMPLE},asplit=2[m{i}][t{i}];[m{i}]volume={v}[w{i}]")
            } else {
                format!("[{input}:a:{i}]{RESAMPLE}{}[w{i}]", volume_arg(v))
            });
            graph.push(format!("[w{i}][k{i}]{DUCK}[v{i}]"));
        }
    }
}

/// A source's filter argument for its volume, omitted when it is 100 %.
fn volume_arg(v: f32) -> String {
    if (v - 1.0).abs() < 1e-6 { String::new() } else { format!(",volume={v}") }
}

pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Option<std::thread::JoinHandle<String>>,
    /// For `smart`: what ffmpeg sends back, measured until it exits.
    clarity: Option<std::thread::JoinHandle<Option<super::clarity::Report>>>,
}

impl Encoder {
    /// Start ffmpeg on a `w` x `h` video, with `sound` fed through
    /// `write_audio`.
    pub fn spawn(codec: &Codec, w: u32, h: u32, order: PixelOrder, fps: u32, sound: Option<&Sound>, path: &Path) -> Result<Self> {
        let mut cmd = ffmpeg();
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]).args(codec.global_args());
        cmd.args(["-f", "matroska", "-i", "pipe:0"]);
        cmd.args(codec.output_args(w, h));
        let mov = matches!(path.extension().and_then(|e| e.to_str()), Some("mp4" | "mov"));
        if let Some(s) = sound {
            cmd.args(s.args(mov));
        }
        // Variable frame rate: each frame keeps its capture time, to the
        // millisecond (left to itself, ffmpeg picks a time base from its
        // frame-rate guess, which can snap timestamps to a coarse grid).
        cmd.args(["-enc_time_base:v", "1/1000"]);
        cmd.args(["-fps_mode", "passthrough"]);
        if matches!(path.extension().and_then(|e| e.to_str()), Some("mp4" | "mov")) {
            cmd.args(["-movflags", "+faststart"]);
        }
        cmd.arg(path);
        let analyses = sound.is_some_and(Sound::analyses);
        if let Some(s) = sound {
            cmd.args(s.analysis_args());
            cmd.args(s.kept_args(&kept_path(path)));
        }
        // Its own process group: Ctrl-C in the terminal reaches only clipt,
        // which then closes ffmpeg's input so it can finish the file.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let stdout = if analyses { Stdio::piped() } else { Stdio::null() };
        let mut child = cmd.stdin(Stdio::piped()).stdout(stdout).stderr(Stdio::piped()).spawn().context("starting ffmpeg")?;
        let stdin = child.stdin.take();
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let clarity = child.stdout.take().map(|out| measure(out, sound.map_or(0, |s| s.volumes.len())));
        let mut enc = Self { child, stdin, stderr: Some(stderr), clarity };
        let tracks = sound.map_or(0, |s| s.volumes.len());
        enc.send(&super::mkv::header(w, h, order, 1_000_000_000 / fps.max(1) as u64, tracks, super::audio::RATE))?;
        Ok(enc)
    }

    /// Send a frame to be shown `ts_ms` milliseconds into the video.
    /// Timestamps must increase.
    pub fn write(&mut self, frame: &[u8], ts_ms: u64) -> Result<()> {
        zone!("frame to ffmpeg");
        self.send(&super::mkv::block_prefix(super::mkv::VIDEO, ts_ms, frame.len()))?;
        self.send(frame)
    }

    /// Send a chunk of source `i`'s sound, heard `ts_ms` milliseconds into
    /// the video.
    pub fn write_audio(&mut self, i: usize, pcm: &[u8], ts_ms: u64) -> Result<()> {
        zone!("sound to ffmpeg");
        self.send(&super::mkv::block_prefix(super::mkv::audio_track(i), ts_ms, pcm.len()))?;
        self.send(pcm)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().context("encoder already finished")?;
        if stdin.write_all(bytes).is_err() {
            // ffmpeg died; its stderr says why.
            self.stdin = None;
            let _ = self.child.wait();
            bail!("ffmpeg stopped: {}", self.stderr_text());
        }
        Ok(())
    }

    /// Close the input and wait for ffmpeg to write out the file. Killed and
    /// reported if it hasn't exited within `FINISH_LIMIT`: the caller says
    /// where the partial file is. For `smart`, returns how clearly the voice
    /// came through.
    pub fn finish(mut self) -> Result<Option<super::clarity::Report>> {
        zone!("ffmpeg finish");
        drop(self.stdin.take());
        let Some(status) = wait_up_to(&mut self.child, FINISH_LIMIT)? else {
            kill(&mut self.child);
            bail!("ffmpeg didn't finish within {FINISH_LIMIT:?}, so it was killed: {}", self.stderr_text());
        };
        if !status.success() {
            bail!("ffmpeg failed ({status}): {}", self.stderr_text());
        }
        // ffmpeg has exited, so its output is closed and the measuring ends.
        Ok(self.clarity.take().and_then(|h| h.join().ok()).flatten())
    }

    fn stderr_text(&mut self) -> String {
        let s = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();
        let s = s.trim();
        if s.is_empty() { "no error output".into() } else { s.lines().last().unwrap_or(s).to_string() }
    }
}

/// Where `smart` keeps the sound as recorded, for the re-mix: beside the
/// video, so it's on the same disk and the re-mixed file can replace it.
pub fn kept_path(video: &Path) -> std::path::PathBuf {
    let name = video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    video.with_file_name(format!(".{name}.clipt-sound.mka"))
}

/// Mix the sound of `video` again from the sound kept beside it, with every
/// source but the voice `down` dB quieter: the video is copied, the mix
/// (and any separate tracks) encoded again from the kept sound, and the
/// result measured as the recording was. Replaces `video` only once ffmpeg
/// has finished; killed and reported if it takes longer than `duration`
/// suggests.
pub fn remix(sound: &Sound, video: &Path, down: f32, duration: Duration) -> Result<Option<super::clarity::Report>> {
    let kept = kept_path(video);
    let name = video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let out = video.with_file_name(format!(".{name}.clipt-remix.{}", video.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
    let mov = matches!(video.extension().and_then(|e| e.to_str()), Some("mp4" | "mov"));
    let quieter = sound.quieter(down);
    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video).arg("-i").arg(&kept);
    cmd.args(quieter.graph_args(1, mov, false)).args(["-c:v", "copy"]);
    if mov {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(&out).args(quieter.analysis_args());
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("starting ffmpeg")?;
    let clarity = child.stdout.take().map(|o| measure(o, sound.volumes.len()));
    let mut err = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    // Sound only, so much faster than real time; this is generous.
    let limit = Duration::from_secs(60) + duration / 2;
    let status = wait_up_to(&mut child, limit)?;
    if status.is_none() {
        kill(&mut child);
    }
    let stderr = stderr.join().unwrap_or_default();
    let report = clarity.and_then(|h| h.join().ok()).flatten();
    match status {
        Some(s) if s.success() => {}
        Some(s) => {
            let _ = std::fs::remove_file(&out);
            bail!("re-mixing failed ({s}): {}", stderr.trim().lines().last().unwrap_or("no error output"));
        }
        None => {
            let _ = std::fs::remove_file(&out);
            bail!("re-mixing didn't finish within {limit:?}, so it was stopped");
        }
    }
    std::fs::rename(&out, video).context("replacing the recording with the re-mix")?;
    Ok(report)
}

/// What the first `loudnorm` pass measured: the fields its second pass
/// asks for, parsed from the JSON it prints.
#[derive(Debug)]
struct Measured {
    i: f64,
    tp: f64,
    lra: f64,
    thresh: f64,
    offset: f64,
}

/// The JSON `loudnorm=print_format=json` leaves on stderr. It is the last
/// thing ffmpeg prints, so its last `{…}` block is it; every value is a
/// string, even the numbers.
fn parse_loudnorm(s: &str) -> Option<Measured> {
    let json = &s[s.rfind('{')?..=s.rfind('}')?];
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let f = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(|x| x.parse().ok());
    Some(Measured { i: f("input_i")?, tp: f("input_tp")?, lra: f("input_lra")?, thresh: f("input_thresh")?, offset: f("target_offset")? })
}

/// The first loudness pass: `loudnorm` reads the mix and reports its
/// loudness as JSON on stderr, meant for exactly this two-pass use.
fn measure_loudness(video: &Path) -> Result<Measured> {
    let out = ffmpeg()
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(video)
        // The null muxer still encodes, by default as `pcm_s16le`, which the
        // Flatpak's ffmpeg doesn't have; `pcm_f32le` it has, for `smart`.
        .args(["-map", "0:a:0", "-af", "loudnorm=I=-14:TP=-1:print_format=json", "-c:a", "pcm_f32le", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running ffmpeg")?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("measuring the mix's loudness failed: {}", err.trim().lines().last().unwrap_or("no error output"));
    }
    parse_loudnorm(&err).with_context(|| {
        format!("measuring the mix's loudness failed: {}", err.trim().lines().last().unwrap_or("loudnorm reported nothing"))
    })
}

/// Normalize the finished mix to -14 LUFS, its true peak at most -1 dBTP
/// (EBU R128, what streaming services normalize to). `loudnorm` measures
/// it and then, given the measurement, applies one fixed gain, which unlike
/// its dynamic mode doesn't pull quiet and loud moments together. The video
/// and any separate tracks are copied; the mix alone is encoded again.
/// Returns the loudness the mix had, or `None` when there was nothing to do:
/// silence has no loudness, and a mix already within half a LU of the
/// target, with no peak over the limit, is left as it is rather than
/// re-encoded.
pub fn loudness(video: &Path, duration: Duration) -> Result<Option<f64>> {
    let m = measure_loudness(video)?;
    if !m.i.is_finite() || ((m.i + 14.0).abs() <= 0.5 && m.tp <= -1.0) {
        return Ok(None);
    }
    let name = video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let out = video.with_file_name(format!(".{name}.clipt-loud.{}", video.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
    let mov = matches!(video.extension().and_then(|e| e.to_str()), Some("mp4" | "mov"));
    let apply = format!(
        "loudnorm=I=-14:TP=-1:measured_I={:.3}:measured_TP={:.3}:measured_LRA={:.3}:measured_thresh={:.3}:offset={:.3}:linear=true",
        m.i, m.tp, m.lra, m.thresh, m.offset
    );
    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video);
    // The mix is the first sound stream: it alone is filtered and encoded
    // again, so separate tracks stay as recorded. The generic `-c:a copy`
    // comes first: an option that matches a stream overrides an earlier
    // one, specific or not.
    cmd.args(["-map", "0", "-c:v", "copy", "-c:a", "copy"]);
    cmd.args(["-filter:a:0", &apply, "-c:a:0", "aac", "-b:a:0", "160k"]);
    if mov {
        cmd.args(["-movflags", "+faststart"]);
    }
    let mut child = cmd.arg(&out).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("running ffmpeg")?;
    let mut err = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    // Sound only, so much faster than real time; this is generous.
    let limit = Duration::from_secs(60) + duration / 2;
    let status = wait_up_to(&mut child, limit)?;
    if status.is_none() {
        kill(&mut child);
    }
    let stderr = stderr.join().unwrap_or_default();
    match status {
        Some(s) if s.success() => {}
        Some(s) => {
            let _ = std::fs::remove_file(&out);
            bail!("normalizing the mix's loudness failed ({s}): {}", stderr.trim().lines().last().unwrap_or("no error output"));
        }
        None => {
            let _ = std::fs::remove_file(&out);
            bail!("normalizing the mix's loudness didn't finish within {limit:?}, so it was stopped");
        }
    }
    std::fs::rename(&out, video).context("replacing the recording with the normalized one")?;
    Ok(Some(m.i))
}

/// Read what ffmpeg sends back for `smart` and measure it. The reading
/// thread only reads, handing blocks to the measuring one, so ffmpeg (which
/// stalls when its output pipe is full, holding up the recording) is never
/// kept waiting by the measurement.
fn measure(mut out: std::process::ChildStdout, channels: usize) -> std::thread::JoinHandle<Option<super::clarity::Report>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let mut a = super::clarity::Analyzer::new(channels);
        // Reads don't end on a frame's boundary; keep the partial one.
        let frame = 4 * channels.max(1);
        let mut carry = Vec::new();
        let mut samples = Vec::new();
        for block in rx {
            carry.extend_from_slice(&block);
            let whole = carry.len() / frame * frame;
            samples.clear();
            samples.extend(carry[..whole].as_chunks::<4>().0.iter().map(|&b| f32::from_le_bytes(b)));
            carry.drain(..whole);
            a.push(&samples);
        }
        a.report()
    })
}

/// Turn a finished recording into a GIF: a palette made from the whole clip,
/// then the clip mapped onto it, at most `fps` frames a second. `duration` is
/// the recording's length; it scales how long ffmpeg may take. Killed and
/// reported if it takes longer, leaving the MP4 for the caller.
pub fn to_gif(mp4: &Path, gif: &Path, fps: u32, duration: Duration) -> Result<()> {
    let filter =
        format!("fps={fps},split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle");
    let mut child = ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(mp4)
        .args(["-vf", &filter, "-loop", "0"])
        .arg(gif)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;
    // Read its errors on a thread, so a chatty ffmpeg can't fill the pipe and
    // block while we wait for it.
    let mut err = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let limit = gif_limit(duration);
    let status = wait_up_to(&mut child, limit)?;
    if status.is_none() {
        kill(&mut child);
    }
    let stderr = stderr.join().unwrap_or_default();
    let Some(status) = status else {
        bail!("making the GIF failed: ffmpeg didn't finish within {limit:?}, so it was killed");
    };
    if !status.success() {
        bail!("making the GIF failed: {}", stderr.trim());
    }
    Ok(())
}

fn debug(msg: &str) {
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// The ffmpeg tests must not overlap: the stand-in ffmpeg goes first on
    /// the process-wide PATH.
    static FFMPEG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    /// A stand-in `ffmpeg` first on PATH that ignores its input and hangs,
    /// like the ffmpeg that stopped at the end of its input in the audio
    /// spike. It writes its pid to a file in its temp dir, which is removed
    /// and PATH restored when dropped.
    struct HungFfmpeg {
        dir: std::path::PathBuf,
        pid: std::path::PathBuf,
        path: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl HungFfmpeg {
        /// The caller must hold `FFMPEG`.
        fn install() -> Self {
            let dir = std::env::temp_dir().join(format!("clipt-hung-ffmpeg-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let pid = dir.join("ffmpeg.pid");
            let script = dir.join("ffmpeg");
            std::fs::write(&script, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n", pid.display())).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::var_os("PATH");
            let mut dirs = vec![dir.clone()];
            dirs.extend(path.iter().flat_map(|p| std::env::split_paths(p)));
            // Safe: every test that runs ffmpeg takes `FFMPEG` first.
            unsafe { std::env::set_var("PATH", std::env::join_paths(dirs).unwrap()) };
            Self { dir, pid, path }
        }

        /// Wait until the stand-in has started, and return its pid.
        fn wait_started(&self) -> u32 {
            let start = Instant::now();
            while !self.pid.exists() {
                assert!(start.elapsed() < Duration::from_secs(10), "the stand-in ffmpeg never started");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::read_to_string(&self.pid).unwrap().trim().parse().unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for HungFfmpeg {
        fn drop(&mut self) {
            unsafe {
                match &self.path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    /// The stand-in was killed, not left running.
    fn assert_gone(pid: u32) {
        // Signal 0 checks for the process without sending it anything.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(!alive, "ffmpeg (pid {pid}) is still running");
    }

    #[test]
    #[cfg(unix)]
    fn finish_kills_a_hung_ffmpeg() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        let fake = HungFfmpeg::install();
        let enc = Encoder::spawn(&Codec::X264, 16, 16, PixelOrder::Bgrx, 30, None, Path::new("/nonexistent/out.mp4")).unwrap();
        let pid = fake.wait_started();
        let start = Instant::now();
        let e = enc.finish().unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(10), "finish waited too long");
        assert!(format!("{e:#}").contains("didn't finish"), "{e:#}");
        assert_gone(pid);
    }

    #[test]
    #[cfg(unix)]
    fn to_gif_kills_a_hung_ffmpeg_and_keeps_the_mp4() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        let fake = HungFfmpeg::install();
        let mp4 = fake.dir.join("in.mp4");
        let gif = fake.dir.join("out.gif");
        std::fs::write(&mp4, b"a recording").unwrap();
        let start = Instant::now();
        let e = to_gif(&mp4, &gif, 10, Duration::from_secs(5)).unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(10), "to_gif waited too long");
        assert!(format!("{e:#}").contains("didn't finish"), "{e:#}");
        assert_gone(fake.wait_started());
        assert!(mp4.exists(), "the MP4 must be kept");
    }

    #[test]
    fn x264_preset_drops_above_1440p() {
        let a = Codec::X264.output_args(1920, 1080);
        assert!(a.contains(&"veryfast".to_string()));
        let a = Codec::X264.output_args(3840, 2160);
        assert!(a.contains(&"ultrafast".to_string()));
    }

    #[test]
    fn every_codec_pads_to_even_size() {
        for c in [Codec::Vaapi("/dev/dri/renderD128".into()), Codec::Nvenc, Codec::X264] {
            let vf = c.output_args(101, 99);
            let i = vf.iter().position(|a| a == "-vf").unwrap();
            assert!(vf[i + 1].starts_with("pad=ceil(iw/2)*2:ceil(ih/2)*2"), "{c:?}: {}", vf[i + 1]);
        }
    }

    /// The AAC check's own command line is valid (it once wasn't, which made
    /// every sound recording fail the check). Needs ffmpeg; passes without.
    #[test]
    fn aac_check_runs() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        check_aac().unwrap();
    }

    #[test]
    fn sound_graphs() {
        let s = |v: &[f32], separate| Sound {
            volumes: v.to_vec(),
            labels: v.iter().enumerate().map(|(i, _)| format!("S{i}")).collect(),
            separate,
            voice: None,
            balance: Balance::Off,
        };
        let graph = |s: &Sound| s.args(false)[1].clone();
        let maps = |s: &Sound| s.args(false).windows(2).filter(|w| w[0] == "-map").map(|w| w[1].clone()).collect::<Vec<_>>();
        // One source: no mixer, and a volume only when it isn't 100 %.
        let one = s(&[1.0], true);
        assert_eq!(graph(&one), format!("[0:a:0]{RESAMPLE}[v0]"));
        assert_eq!(maps(&one), ["0:v", "[v0]"], "a lone source isn't kept twice");
        assert!(graph(&s(&[0.5], false)).ends_with(",volume=0.5[v0]"));
        // Two: each at its volume into the mixer.
        let two = s(&[0.7, 1.2], false);
        assert_eq!(graph(&two), format!("[0:a:0]{RESAMPLE},volume=0.7[v0];[0:a:1]{RESAMPLE},volume=1.2[v1];[v0][v1]{MIX}:inputs=2[mix]"));
        assert_eq!(maps(&two), ["0:v", "[mix]"]);
        // Separate tracks: split before the volume, so they're as recorded.
        let sep = s(&[0.7, 1.0], true);
        assert!(graph(&sep).starts_with(&format!("[0:a:0]{RESAMPLE},asplit=2[m0][t0];[m0]volume=0.7[v0];")));
        assert_eq!(maps(&sep), ["0:v", "[mix]", "[t0]", "[t1]"]);
        assert!(sep.args(false).windows(2).any(|w| w == ["-metadata:s:a:2", "title=S1"]));
        assert!(sep.args(true).windows(2).any(|w| w == ["-metadata:s:a:0", "handler_name=Mix"]), "MP4 names tracks by handler");
    }

    #[test]
    fn duck_graphs() {
        let s = |v: &[f32], separate, voice, balance| Sound {
            volumes: v.to_vec(),
            labels: v.iter().enumerate().map(|(i, _)| format!("S{i}")).collect(),
            separate,
            voice,
            balance,
        };
        let graph = |s: &Sound| s.args(false)[1].clone();
        let maps = |s: &Sound| s.args(false).windows(2).filter(|w| w[0] == "-map").map(|w| w[1].clone()).collect::<Vec<_>>();
        // The microphone is cleaned up into the mix and split into the
        // ducking keys; the other source is sidechained after its volume.
        let duck = s(&[0.7, 1.0], false, Some(1), Balance::Duck);
        assert_eq!(
            graph(&duck),
            format!(
                "[0:a:1]{RESAMPLE}[wet1];{};[m1]anull[v1];\
                 [0:a:0]{RESAMPLE},volume=0.7[w0];[w0][k0]{DUCK}[v0];[v0][v1]{MIX}:inputs=2[mix]",
                clean_voice(1, "[m1][k0]")
            )
        );
        assert_eq!(maps(&duck), ["0:v", "[mix]"]);
        // Separate tracks stay as recorded: the microphone is split before
        // the clean-up, and the other source before its volume.
        let sep = s(&[0.7, 1.0], true, Some(1), Balance::Duck);
        assert_eq!(
            graph(&sep),
            format!(
                "[0:a:1]{RESAMPLE},asplit=2[wet1][t1];{};[m1]anull[v1];\
                 [0:a:0]{RESAMPLE},asplit=2[m0][t0];[m0]volume=0.7[w0];[w0][k0]{DUCK}[v0];[v0][v1]{MIX}:inputs=2[mix]",
                clean_voice(1, "[m1][k0]")
            )
        );
        assert_eq!(maps(&sep), ["0:v", "[mix]", "[t0]", "[t1]"]);
        // `off` is the graph without any of that.
        let off = s(&[0.7, 1.0], false, Some(1), Balance::Off);
        assert!(!graph(&off).contains(CLEAN_MIC) && !graph(&off).contains(DUCK));
        // Without a microphone, `duck` changes nothing.
        let no_mic = s(&[0.7, 1.0], false, None, Balance::Duck);
        assert_eq!(no_mic.args(false), s(&[0.7, 1.0], false, None, Balance::Off).args(false));
        // Nor with the microphone alone.
        let mic_only = s(&[1.0], false, Some(0), Balance::Duck);
        assert_eq!(graph(&mic_only), format!("[0:a:0]{RESAMPLE}[v0]"));
        assert_eq!(maps(&mic_only), ["0:v", "[v0]"]);
        // Several other sources each get their own key.
        let three = s(&[1.0, 1.0, 1.0], false, Some(1), Balance::Duck);
        assert!(graph(&three).contains("[key]asplit=2[k0][k2]"), "{}", graph(&three));
        assert_eq!(Balance::parse("off").unwrap(), Balance::Off);
        assert_eq!(Balance::parse("duck").unwrap(), Balance::Duck);
        assert_eq!(Balance::parse("smart").unwrap(), Balance::Smart);
        assert!(Balance::parse("loud").is_err());
        // `smart` is `duck` with each input to the mix also sent back, and
        // the sources as recorded kept, as for separate tracks.
        let smart = s(&[0.7, 1.0], false, Some(1), Balance::Smart);
        assert!(graph(&smart).starts_with(&graph(&sep).replace(&format!(";[v0][v1]{MIX}:inputs=2[mix]"), "")), "{}", graph(&smart));
        assert_eq!(maps(&smart), ["0:v", "[mix]"], "kept, not as tracks");
        let kept = |s: &Sound| s.kept_args(Path::new("k.mka")).iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(kept(&smart), ["-map", "[t0]", "-map", "[t1]", "-c:a", "flac", "-f", "matroska", "k.mka"]);
        // With separate tracks as well, each source is split for both.
        let smart_sep = s(&[0.7, 1.0], true, Some(1), Balance::Smart);
        assert!(graph(&smart_sep).contains("[t0]asplit=2[s0][r0];[t1]asplit=2[s1][r1]"), "{}", graph(&smart_sep));
        assert_eq!(maps(&smart_sep), ["0:v", "[mix]", "[s0]", "[s1]"]);
        assert_eq!(kept(&smart_sep)[..4], ["-map", "[r0]", "-map", "[r1]"]);
        assert!(kept(&duck).is_empty());
        // The re-mix reads the kept sound (input 1), the others turned down.
        let down = smart.quieter(6.0);
        assert!((down.volumes[0] - 0.7 * 0.501).abs() < 0.001 && down.volumes[1] == 1.0, "{:?}", down.volumes);
        let remix = down.graph_args(1, false, false)[1].clone();
        assert!(remix.starts_with("[1:a:1]") && remix.contains("[1:a:0]") && !remix.contains("[0:a:"), "{remix}");
        assert!(graph(&smart).ends_with(&format!("[z0][z1]amerge=inputs=2,aformat=sample_fmts=flt[ana];[u0][u1]{MIX}:inputs=2[mix]")));
        assert!(graph(&smart).contains("[v1]asplit=2[u1][y1];[y1]pan=mono|c0=0.5*c0+0.5*c1[z0]"), "the voice comes first");
        assert_eq!(smart.analysis_args(), ["-map", "[ana]", "-c:a", "pcm_f32le", "-f", "f32le", "pipe:1"]);
        assert!(duck.analysis_args().is_empty());
        assert!(!s(&[1.0, 1.0], false, None, Balance::Smart).analyses(), "no microphone, nothing to measure");
    }

    /// Mean level (dBFS) of a lavfi `source` through a filter graph from
    /// `[0:a]` to `[o]`, as volumedetect reports it.
    fn mean_volume(source: &str, graph: &str) -> f64 {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-nostats", "-f", "lavfi", "-i", source])
            .args(["-filter_complex", &format!("{graph};[o]volumedetect"), "-f", "null", "-"])
            .stdin(Stdio::null())
            .output()
            .expect("running ffmpeg");
        assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
        let err = String::from_utf8_lossy(&out.stderr);
        err.lines()
            .find_map(|l| l.split_once("mean_volume:").map(|(_, v)| v.split_whitespace().next().unwrap_or("").to_string()))
            .and_then(|v| v.parse().ok())
            .expect("volumedetect reported no mean level")
    }

    /// The clean-up leaves the voice at its recorded level, quiet or loud:
    /// within 1 dB of the raw voice from -45 to -23 dBFS mean on speech-like
    /// sound. The fixed 9.5 dB make-up this replaces was right at -23 but
    /// 9 dB too loud at -45 (and on LibriVox readings, 10 dB at -45, 7 at
    /// -35; this chain is within 0.1 dB of them throughout).
    #[test]
    fn duck_cleanup_keeps_the_voice_level() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        // A voiced vowel, modulated at a syllable rate with pauses: the
        // speech-shaped stand-in for the spike's reading, at -23 dBFS mean.
        let vowel = "(0.152*sin(2*PI*130*t)+0.091*sin(2*PI*260*t)+0.053*sin(2*PI*520*t)+0.030*sin(2*PI*1040*t)+0.015*sin(2*PI*2080*t))*if(lt(mod(t,0.82),0.2),1,if(lt(mod(t,0.82),0.5),0.3,0.05))";
        let voice = format!("aevalsrc='{vowel}|{vowel}':s=48000:d=10");
        for gain in [-22, -12, 0] {
            let raw = mean_volume(&voice, &format!("[0:a]volume={gain}dB[o]"));
            let cleaned = mean_volume(&voice, &format!("[0:a]volume={gain}dB[wet0];{};[x]anullsink", clean_voice(0, "[o][x]")));
            assert!(
                (raw - cleaned).abs() < 1.0,
                "cleaned voice is {:.2} dB from the raw one ({raw:.1} -> {cleaned:.1} dBFS)",
                cleaned - raw
            );
        }
    }

    /// The `smart` graph through ffmpeg: a voice with pauses over a bed 20 dB
    /// under it comes back measured, as clear and not leaking.
    #[test]
    fn smart_measures_what_goes_into_the_mix() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("clipt-smart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mkv");
        // Source 0: pink noise as the bed; source 1: the microphone, noise
        // in syllables with pauses. 16-bit stereo, as clipt sends.
        let made = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=gray:s=64x64:d=12"])
            .args(["-f", "lavfi", "-i", "anoisesrc=c=pink:a=0.02:d=12:r=48000"])
            .args(["-f", "lavfi", "-i", "anoisesrc=c=pink:a=0.2:d=12:r=48000:seed=7"])
            .args(["-filter_complex", "[2:a]volume='if(lt(mod(t,4),2.8),if(lt(mod(t,0.4),0.25),1,0.3),0)':eval=frame[mic]"])
            .args(["-map", "0:v", "-map", "1:a", "-map", "[mic]", "-ac", "2", "-c:v", "rawvideo", "-c:a", "pcm_s16le"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(made.success());
        let smart = Sound {
            volumes: vec![1.0, 1.0],
            labels: vec!["System".into(), "Microphone".into()],
            separate: false,
            voice: Some(1),
            balance: Balance::Smart,
        };
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&input)
            .args(smart.args(false))
            .args(["-c:v", "rawvideo"])
            .arg(dir.join("out.mkv"))
            .args(smart.analysis_args())
            .args(smart.kept_args(&dir.join("kept.mka")))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let samples: Vec<f32> = out.stdout.as_chunks::<4>().0.iter().map(|&b| f32::from_le_bytes(b)).collect();
        assert!((samples.len() as f32 / 2.0 / 48_000.0 - 12.0).abs() < 0.5, "{} samples", samples.len());
        let mut a = super::super::clarity::Analyzer::new(2);
        a.push(&samples);
        let r = a.report().unwrap();
        // Ducked about 11 dB, the bed ends up well under a voice it was
        // 20 dB under: clear.
        use super::super::clarity::{Leak, Report};
        assert!(matches!(r, Report::Measured { clarity, leak: Leak::NotHeard, .. } if clarity > 0.9), "{r:?}");
    }

    /// A voice under a bed 10 dB louder: `smart` measures it below the
    /// target and says how much quieter the bed should be; the re-mix at that
    /// reaches the target, keeps the video, and replaces the file.
    #[test]
    fn smart_remix_reaches_the_target() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        use super::super::clarity::{Analyzer, Report};
        let dir = std::env::temp_dir().join(format!("clipt-remix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mkv");
        let made = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=gray:s=64x64:d=12"])
            .args(["-f", "lavfi", "-i", "anoisesrc=c=pink:a=0.6:d=12:r=48000"])
            .args(["-f", "lavfi", "-i", "anoisesrc=c=pink:a=0.2:d=12:r=48000:seed=7"])
            .args(["-filter_complex", "[2:a]volume='if(lt(mod(t,4),2.8),if(lt(mod(t,0.4),0.25),1,0.3),0)':eval=frame[mic]"])
            .args(["-map", "0:v", "-map", "1:a", "-map", "[mic]", "-ac", "2", "-c:v", "rawvideo", "-c:a", "pcm_s16le"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(made.success());
        let smart = Sound {
            volumes: vec![1.0, 1.0],
            labels: vec!["System".into(), "Microphone".into()],
            separate: true,
            voice: Some(1),
            balance: Balance::Smart,
        };
        // As Encoder::spawn runs it: the video, the measurement, the kept sound.
        let video = dir.join("out.mkv");
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&input)
            .args(smart.args(false))
            .args(["-c:v", "rawvideo"])
            .arg(&video)
            .args(smart.analysis_args())
            .args(smart.kept_args(&kept_path(&video)))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let samples: Vec<f32> = out.stdout.as_chunks::<4>().0.iter().map(|&b| f32::from_le_bytes(b)).collect();
        let mut a = Analyzer::new(2);
        a.push(&samples);
        let before = a.report().unwrap();
        let Report::Measured { clarity, quieter: Some((down, _)), .. } = before else { panic!("{before:?}") };
        assert!(clarity < 0.75, "{before:?}");
        let after = remix(&smart, &video, down, Duration::from_secs(12));
        let streams = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(&video)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let after = after.unwrap().unwrap();
        assert!(matches!(after, Report::Measured { clarity, .. } if clarity >= 0.75), "{before:?} -> {after:?}");
        assert_eq!(
            String::from_utf8_lossy(&streams.stdout).lines().collect::<Vec<_>>(),
            ["video", "audio", "audio", "audio"],
            "video, mix, two tracks"
        );
    }

    /// The mix normalized to -14 LUFS, any separate tracks copied as they
    /// were, and a second call finding nothing to do.
    #[test]
    fn loudness_normalizes_the_mix() {
        let _ffmpeg = FFMPEG.lock().unwrap_or_else(|e| e.into_inner());
        if Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("clipt-loud-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A quiet mix and a separate track at its recorded level.
        let video = dir.join("out.mkv");
        let made = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=gray:s=64x64:d=6"])
            .args(["-f", "lavfi", "-i", "sine=frequency=1000:r=48000:d=6"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:r=48000:d=6"])
            .args(["-filter_complex", "[1:a]volume=0.05[mix];[2:a]volume=0.3[sep]"])
            .args(["-map", "0:v", "-map", "[mix]", "-map", "[sep]", "-ac", "2", "-c:v", "rawvideo", "-c:a", "pcm_s16le"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success());
        // The mean level of one sound stream, as volumedetect reports it.
        let level = |select: &str| -> f64 {
            let out = Command::new("ffmpeg")
                .args(["-hide_banner", "-nostats", "-i"])
                .arg(&video)
                .args(["-map", select, "-af", "volumedetect", "-f", "null", "-"])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .find_map(|l| l.split_once("mean_volume:").map(|(_, v)| v.split_whitespace().next().unwrap_or("").to_string()))
                .and_then(|v| v.parse().ok())
                .unwrap()
        };
        // The mix's loudness, as the first pass measures it.
        let lufs = || measure_loudness(&video).unwrap().i;
        let sep_was = level("0:a:1");
        let was = loudness(&video, Duration::from_secs(6)).unwrap().unwrap();
        assert!(was < -20.0, "the mix started quiet ({was} LUFS)");
        assert!((lufs() + 14.0).abs() < 0.5, "the mix is now at -14 LUFS ({:.2})", lufs());
        // The separate track was copied, not re-encoded, at its level.
        assert!((level("0:a:1") - sep_was).abs() < 0.1, "the separate track changed ({sep_was} -> {})", level("0:a:1"));
        let codecs = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
            .arg(&video)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&codecs.stdout).lines().collect::<Vec<_>>(),
            ["aac", "pcm_s16le"],
            "the mix encoded, the track copied"
        );
        // Already at the target: a second call has nothing to do.
        assert!(loudness(&video, Duration::from_secs(6)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The measurement is in the last JSON block on stderr, where the rest of
    /// ffmpeg's chatter can't reach it, its numbers strings.
    #[test]
    fn loudnorm_report_is_parsed() {
        let s = concat!(
            "[Parsed_loudnorm_0 @ 0x55] \n{\n",
            "\t\"input_i\" : \"-23.45\",\n",
            "\t\"input_tp\" : \"-10.02\",\n",
            "\t\"input_lra\" : \"3.10\",\n",
            "\t\"input_thresh\" : \"-33.71\",\n",
            "\t\"output_i\" : \"-14.00\",\n",
            "\t\"target_offset\" : \"0.05\"\n}\n",
            "size=N/A time=00:00:06.00 bitrate=N/A\n",
        );
        let m = parse_loudnorm(s).unwrap();
        assert_eq!((m.i, m.tp, m.lra, m.thresh, m.offset), (-23.45, -10.02, 3.10, -33.71, 0.05));
        // Silence: nothing to parse.
        assert!(parse_loudnorm("Error opening output files: Invalid argument").is_none());
    }

    #[test]
    fn codec_preferences() {
        assert_eq!(candidates("x264").unwrap(), vec![Codec::X264]);
        assert_eq!(candidates("nvenc").unwrap(), vec![Codec::Nvenc]);
        if cfg!(windows) {
            assert!(candidates("auto").unwrap().ends_with(&[Codec::Mf, Codec::X264]));
            assert!(candidates("vaapi").is_err());
        } else {
            assert!(candidates("auto").unwrap().ends_with(&[Codec::Nvenc, Codec::X264]));
            assert!(candidates("qsv").is_err());
        }
        assert!(candidates("av1").is_err());
    }
}
