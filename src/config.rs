//! ~/.config/clipt/config.toml — every key optional.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where Ctrl+S / `full` / `screen` save. `~` is expanded.
    pub save_dir: String,
    /// strftime pattern, without extension.
    pub filename: String,
    /// Starting colour, 1-8 (red, orange, yellow, green, blue, purple, white, black).
    pub color: usize,
    /// Colours 1-8 as "#rrggbb". Right-clicking one in the editor changes it
    /// and writes this back.
    pub palette: Vec<String>,
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
    /// mp4 or gif (converted when the recording stops, at most 15 fps).
    pub record_format: String,
    /// Sound to record: none, system, mic, both, or a list of sources with
    /// volumes, as `--audio` takes it.
    pub record_audio: String,
    /// Duck the other sound under the microphone: off, duck or smart.
    pub record_audio_balance: String,
    /// Also keep each sound source as its own track.
    pub record_separate_audio: bool,
    /// A recording stops by itself after this many seconds (0 = no limit).
    pub max_duration: u64,
    /// macOS and Windows: the tray's shortcut to capture a region, such as
    /// "cmd+shift+2" (macOS) or "print" (Windows); "" for none.
    pub hotkey: String,
    /// macOS and Windows: the shortcut to copy the whole desktop ("" for none).
    pub hotkey_full: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            save_dir: "~/Pictures/Screenshots".into(),
            filename: "Screenshot_%Y-%m-%d_%H-%M-%S".into(),
            color: 1,
            palette: crate::editor::PALETTE.iter().map(|&c| hex(c)).collect(),
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
            record_format: "mp4".into(),
            record_audio: "none".into(),
            record_audio_balance: "off".into(),
            record_separate_audio: false,
            max_duration: 3600,
            hotkey: if cfg!(windows) { "print" } else { "cmd+shift+2" }.into(),
            hotkey_full: String::new(),
        }
    }
}

impl Config {
    #[cfg(windows)]
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData/Roaming"));
        base.join("clipt").join("config.toml")
    }

    #[cfg(not(windows))]
    pub fn path() -> PathBuf {
        config_home().join("clipt/config.toml")
    }

    pub fn load() -> anyhow::Result<Self> {
        let p = Self::path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Colours 1-8: the defaults, with any the config sets in their place.
    pub fn palette_rgb(&self) -> [(u8, u8, u8); 8] {
        let mut out = crate::editor::PALETTE;
        for (slot, s) in out.iter_mut().zip(&self.palette) {
            match parse_hex(s) {
                Some(c) => *slot = c,
                None => eprintln!("clipt: palette: '{s}' isn't a colour like \"#ef4444\"; using the default"),
            }
        }
        out
    }

    /// Write `palette` into the config file, leaving the rest of it alone.
    pub fn save_palette(palette: &[(u8, u8, u8); 8]) -> anyhow::Result<()> {
        let v = palette.iter().map(|&c| format!("\"{}\"", hex(c))).collect::<Vec<_>>().join(", ");
        Self::set_raw("palette", &format!("[{v}]"))
    }

    /// Set `key` to `value` in the config file, keeping its comments and
    /// other keys. `value` is a TOML value (`true`, `30`, `"gif"`, `[...]`);
    /// anything that isn't one counts as a string, so `gif` works unquoted.
    /// Refused, leaving the file alone, if the result isn't a valid config.
    pub fn set(key: &str, value: &str) -> anyhow::Result<()> {
        let literal = format!("v = {value}");
        let value = if toml::from_str::<toml::Table>(&literal).is_ok() { value.to_string() } else { toml_string(value) };
        Self::set_raw(key, &value)
    }

    fn set_raw(key: &str, value: &str) -> anyhow::Result<()> {
        let p = Self::path();
        let old = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let new = with_key(&old, key, value);
        let cfg = toml::from_str::<Config>(&new).map_err(|e| anyhow::anyhow!("{key} = {value}: {}", e.message()))?;
        anyhow::ensure!(matches!(cfg.record_format.as_str(), "mp4" | "gif"), "record_format is mp4 or gif");
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("toml.new");
        std::fs::write(&tmp, new)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Every setting and its value, as `key = value` lines (TOML).
    pub fn listing(&self) -> String {
        let serde_json::Value::Object(map) = serde_json::to_value(self).unwrap_or_default() else { return String::new() };
        map.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| format!("{k} = {v}\n")).collect()
    }

    /// One setting's value, as TOML.
    pub fn get(&self, key: &str) -> Option<String> {
        let serde_json::Value::Object(map) = serde_json::to_value(self).ok()? else { return None };
        map.get(key).filter(|v| !v.is_null()).map(|v| v.to_string())
    }

    /// A fresh, non-clobbering path in `save_dir`.
    pub fn next_save_path(&self) -> PathBuf {
        fresh_path(&expand(&self.save_dir), &self.filename, "png")
    }

    /// A fresh, non-clobbering path in `video_dir`, with `ext` (mp4, gif).
    pub fn next_video_path(&self, ext: &str) -> PathBuf {
        fresh_path(&expand(&self.video_dir), &self.video_filename, ext)
    }
}

pub fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let n = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((n(0)?, n(2)?, n(4)?))
}

/// A TOML string holding `s`.
fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// `src` (a config file's text) with `key = value` in place of the key's
/// line, or added before the first table, or at the end. Only that line,
/// or those lines for an array split over several, change; comments and
/// the other keys stay as they are.
fn with_key(src: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let mut out = Vec::new();
    let mut lines = src.lines();
    let mut done = false;
    while let Some(l) = lines.next() {
        let trimmed = l.trim_start();
        if !done && trimmed.strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('=')) {
            // Skip the rest of an array written over several lines.
            let open = l.split_once('=').map_or("", |(_, v)| v.trim_start());
            let mut cur = l;
            while open.starts_with('[') && !cur.contains(']') {
                match lines.next() {
                    Some(n) => cur = n,
                    None => break,
                }
            }
            out.push(line.clone());
            done = true;
        } else if !done && trimmed.starts_with('[') {
            // A table starts: top-level keys must come before it.
            out.push(line.clone());
            out.push(l.to_string());
            done = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !done {
        out.push(line);
    }
    out.join("\n") + "\n"
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

/// `$XDG_CONFIG_HOME`, or `~/.config` when it's unset, empty or relative
/// (the XDG spec says to ignore those).
#[cfg(not(windows))]
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".config"))
}

pub fn home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

#[cfg(not(windows))]
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

/// The C runtime's strftime aborts the process on a conversion it doesn't
/// know (its invalid-parameter handler), so Windows gets the common ones
/// here and leaves any other `%x` as it is.
#[cfg(windows)]
fn strftime(fmt: &str) -> String {
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format_time(fmt, &t)
}

#[cfg(windows)]
fn format_time(fmt: &str, t: &windows_sys::Win32::Foundation::SYSTEMTIME) -> String {
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] =
        ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let day = DAYS[t.wDayOfWeek as usize % 7];
    let month = MONTHS[(t.wMonth as usize).clamp(1, 12) - 1];
    let h12 = match t.wHour % 12 {
        0 => 12,
        h => h,
    };
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let s = match chars.next() {
            Some('Y') => t.wYear.to_string(),
            Some('y') => format!("{:02}", t.wYear % 100),
            Some('m') => format!("{:02}", t.wMonth),
            Some('d') => format!("{:02}", t.wDay),
            Some('e') => format!("{:2}", t.wDay),
            Some('H') => format!("{:02}", t.wHour),
            Some('I') => format!("{h12:02}"),
            Some('M') => format!("{:02}", t.wMinute),
            Some('S') => format!("{:02}", t.wSecond),
            Some('p') => (if t.wHour < 12 { "AM" } else { "PM" }).into(),
            Some('a') => day[..3].into(),
            Some('A') => day.into(),
            Some('b') => month[..3].into(),
            Some('B') => month.into(),
            Some('F') => format!("{}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay),
            Some('T') => format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond),
            Some('%') => "%".into(),
            Some(o) => format!("%{o}"),
            None => "%".into(),
        };
        out.push_str(&s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_value(p: &[(u8, u8, u8); 8]) -> String {
        format!("[{}]", p.iter().map(|&c| format!("\"{}\"", hex(c))).collect::<Vec<_>>().join(", "))
    }

    #[test]
    fn settings_are_set_in_place() {
        let src = "# clipt\nfilename = \"shot\"\nrecord_format = \"mp4\" # the default\nnotify = true\n\n[x]\n";
        let out = with_key(src, "record_format", &toml_string("gif"));
        assert_eq!(out, "# clipt\nfilename = \"shot\"\nrecord_format = \"gif\"\nnotify = true\n\n[x]\n");
        let out = with_key(src, "copy_on_save", "false");
        assert!(out.contains("notify = true\n\ncopy_on_save = false\n[x]"), "before the first table: {out}");
        let cfg = Config::default();
        assert_eq!(cfg.get("record_format").as_deref(), Some("\"mp4\""));
        assert_eq!(cfg.get("nope"), None);
        assert!(cfg.listing().lines().any(|l| l == "notify = true"), "{}", cfg.listing());
        let cfg: Config = toml::from_str(&cfg.listing()).expect("the listing reads back as a config");
        assert_eq!(cfg.record_format, "mp4");
    }

    #[test]
    fn palette_round_trips_through_the_file() {
        let pal = crate::editor::PALETTE;
        let mut new = pal;
        new[0] = (1, 2, 3);
        let src = "# mine\ncolor = 2\npalette = [\n  \"#ff0000\",\n]\nwidth = 5\n";
        let out = with_key(src, "palette", &palette_value(&new));
        assert!(out.starts_with("# mine\ncolor = 2\npalette = [\"#010203\", "), "{out}");
        assert!(out.ends_with("]\nwidth = 5\n"), "the other keys stay: {out}");
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.palette_rgb(), new);
        assert_eq!(cfg.color, 2);

        let out = with_key("width = 5\n", "palette", &palette_value(&pal));
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!((cfg.palette_rgb(), cfg.width), (pal, 5.0), "added when missing: {out}");

        let cfg: Config = toml::from_str("palette = [\"#00ff00\", \"nope\"]").unwrap();
        let rgb = cfg.palette_rgb();
        assert_eq!((rgb[0], rgb[1], rgb[2]), ((0, 255, 0), pal[1], pal[2]), "bad and missing entries keep the default");
    }
}
