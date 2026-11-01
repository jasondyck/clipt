//! ~/.config/clipt/config.toml — every key optional.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where Ctrl+S / `full` / `screen` save. `~` is expanded.
    pub save_dir: String,
    /// strftime pattern, without extension.
    pub filename: String,
    /// Starting colour, 1-8 (red, orange, yellow, green, blue, purple, white, black).
    pub color: usize,
    /// Starting stroke width in logical pixels.
    pub width: f32,
    /// Saving from the editor also copies to the clipboard.
    pub copy_on_save: bool,
    /// Desktop notification after saving or copying.
    pub notify: bool,
    /// auto, ext, wlr, kwin or portal.
    pub backend: String,
    /// Include the mouse pointer (backends that support it).
    pub cursor: bool,
    /// Path to a TTF/OTF file; default asks fontconfig for sans-serif.
    pub font: Option<String>,
    /// `clipt tray` checks GitHub for a new release daily (off by default).
    pub check_updates: bool,
    /// Where `clipt record` saves. `~` is expanded.
    pub video_dir: String,
    /// strftime pattern for recordings, without extension.
    pub video_filename: String,
    /// Recording frame rate.
    pub record_fps: u32,
    /// auto, vaapi, nvenc or x264.
    pub record_codec: String,
    /// A recording stops by itself after this many seconds (0 = no limit).
    pub max_duration: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            save_dir: "~/Pictures/Screenshots".into(),
            filename: "Screenshot_%Y-%m-%d_%H-%M-%S".into(),
            color: 1,
            width: 4.0,
            copy_on_save: true,
            notify: true,
            backend: "auto".into(),
            cursor: false,
            font: None,
            check_updates: false,
            video_dir: "~/Videos/Screencasts".into(),
            video_filename: "Recording_%Y-%m-%d_%H-%M-%S".into(),
            record_fps: 30,
            record_codec: "auto".into(),
            max_duration: 3600,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
        base.join("clipt/config.toml")
    }

    pub fn load() -> anyhow::Result<Self> {
        let p = Self::path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// A fresh, non-clobbering path in `save_dir`.
    pub fn next_save_path(&self) -> PathBuf {
        fresh_path(&expand(&self.save_dir), &self.filename, "png")
    }

    /// A fresh, non-clobbering path in `video_dir`.
    pub fn next_video_path(&self) -> PathBuf {
        fresh_path(&expand(&self.video_dir), &self.video_filename, "mp4")
    }
}

fn fresh_path(dir: &std::path::Path, pattern: &str, ext: &str) -> PathBuf {
    let stem = strftime(pattern);
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem}_{n}.{ext}"));
        n += 1;
    }
    p
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

fn strftime(fmt: &str) -> String {
    let Ok(cfmt) = std::ffi::CString::new(fmt) else { return fmt.to_string() };
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 256];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}
