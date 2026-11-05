//! clipt on Windows: screenshots, with the same editor as on Linux and
//! macOS, recording with sound, the tray with a hotkey, and
//! `clipt update`; see docs/windows-plan.md.

mod capture;
mod clipboard;
mod overlay;
pub mod record;
mod tray;
mod windows;

pub use tray::notify;

use crate::config::Config;
use crate::editor::{Editor, Outcome};
use crate::record::Target;
use crate::record::audio::Source;
use crate::{Args, crop_geometry, parse_geometry, stitch, write_png};
use anyhow::{Context, Result, bail};

pub const HELP: &str = "\
clipt — screenshot tool

USAGE:
    clipt [gui]           Select a region and annotate it (default)
    clipt full            Capture the whole desktop
    clipt screen [NAME]   Capture one monitor (default: the main one)
    clipt list            Show the monitors
    clipt list sound      Show the sound devices recordings can use
    clipt record          Pick an area or window to record (Enter starts), saved as MP4
    clipt record screen [NAME]  Record one monitor (default: the main one)
    clipt record window APP     Record APP's frontmost window, e.g. notepad
    clipt record -g GEOM  Record a region (within one monitor)
    clipt record --stop   Stop the running recording
    clipt record --pause / --resume / --toggle-pause
    clipt tray            Notification-area icon, with the hotkey (Print Screen)
    clipt config [KEY [VALUE]]  Show the settings, one setting, or set it
    clipt update          Update to the latest release (--check: only check)

OPTIONS:
    -o, --output PATH     Write the PNG to PATH ('-' for stdout) instead of the save dir
                          (record: the .mp4/.mkv/.mov/.gif file)
    -c, --clipboard       Copy to the clipboard (full/screen)
    -g, --geometry GEOM   Crop to \"X,Y WxH\" (full)
    -d, --delay SECS      Wait before capturing
        --cursor          Include the pointer
        --fps N           Recording frame rate (default 30)
        --gif             Save the recording as a GIF (at most 15 fps)
        --audio[=SOURCES] Record sound too (default: system). SOURCES: system,
                          mic, both, or device names from `clipt list sound`,
                          comma-separated, each with an optional :PERCENT
                          volume in the mix, e.g. system:70,mic:120
        --audio-balance MODE
                          Duck the other sound under the microphone: off,
                          duck, or smart (duck, and say at the stop how
                          clearly the voice came through). Default: off
        --separate-audio  Also keep each sound source as its own track
        --no-audio        Record without sound, whatever record_audio says
    -h, --help            Show this help
    -V, --version         Show the version

EDITOR KEYS:
    drag                  select (click = whole screen; drag handles to resize)
    V R E A L P M T X N   select, rect, ellipse, arrow, line, pen, highlighter,
                          text, pixelate, counter
    1-8                   colour          scroll / [ ]   stroke width
    Shift                 square / 45°    arrows         nudge (Shift resizes)
    Enter, Ctrl+C, dbl-click  copy        Ctrl+S         save
    Ctrl+Z / Ctrl+Shift+Z undo / redo     Esc / right-click  cancel / back
    Ctrl+R                switch between screenshot and video (or click
                          Screenshot / Video at the bottom of the screen)
    S / M (record mode)   computer sound / microphone on or off

Config: %APPDATA%\\clipt\\config.toml (save_dir, filename, color, palette,
width, copy_on_save, notify, cursor, font, hotkey, hotkey_full, video_dir,
video_filename, record_fps, record_codec, record_format, record_audio,
record_audio_balance, record_separate_audio, max_duration).
hotkey is the tray's shortcut for a region, default \"print\" (Print
Screen), or e.g. \"ctrl+shift+s\"; \"\" turns it off. Notifications need
the tray. Screenshots go to Pictures\\Screenshots and recordings to
Videos\\Captures, where Windows keeps its own. Recording needs ffmpeg
(winget install Gyan.FFmpeg, or ffmpeg.exe beside clipt.exe).
";

pub fn run(args: Args, cfg: Config) -> Result<()> {
    crate::update::cleanup();
    match args.cmd.as_str() {
        "gui" | "full" | "screen" | "list" | "tray" => {}
        "update" => return crate::update::cli(args.check),
        "record" => return record_cmd(args, &cfg),
        "gnome-extension" => bail!("`clipt {}` is for GNOME", args.cmd),
        other => bail!("unknown command '{other}' (try --help)"),
    }
    if args.backend.is_some() {
        bail!("--backend: Windows has only the one");
    }
    if matches!(args.cmd.as_str(), "gui" | "tray") && detach()? {
        return Ok(());
    }
    // Physical pixels everywhere, so every monitor is captured and covered
    // at its own resolution, whatever its scale.
    capture::dpi_aware();
    if args.cmd == "tray" {
        return tray::run(&cfg);
    }
    let cursor = args.cursor || cfg.cursor;
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
    }
    if args.cmd == "list" {
        match args.screen.as_deref() {
            Some("sound") => return crate::list_sound(),
            Some(other) => bail!("unknown list '{other}' (clipt list, or clipt list sound)"),
            None => {}
        }
    }
    let desk = capture::Desktop::get()?;
    if args.cmd == "list" {
        for o in &desk.outputs {
            println!("{}\t{}x{}+{}+{}\t{}", o.name, o.native.0, o.native.1, o.x, o.y, o.description);
        }
        return Ok(());
    }

    if args.cmd != "gui" {
        let targets: Vec<usize> = if args.cmd == "screen" {
            match &args.screen {
                Some(n) => vec![
                    desk.outputs.iter().position(|o| &o.name == n).with_context(|| format!("no monitor named '{n}' (see clipt list)"))?,
                ],
                None => vec![0],
            }
        } else if let Some(g) = &args.geometry {
            let (x, y, w, h) = parse_geometry(g)?;
            let hit: Vec<_> = (0..desk.outputs.len())
                .filter(|&i| {
                    let o = &desk.outputs[i];
                    (o.x as f64) < x + w && x < (o.x + o.width as i32) as f64 && (o.y as f64) < y + h && y < (o.y + o.height as i32) as f64
                })
                .collect();
            anyhow::ensure!(!hit.is_empty(), "geometry '{g}' is outside every monitor");
            hit
        } else {
            (0..desk.outputs.len()).collect()
        };
        let shots = desk.capture(&targets, cursor)?;
        let mut img = stitch(&shots);
        if let Some(g) = &args.geometry {
            img = crop_geometry(&img, &shots, g)?;
        }
        let save = args.output.is_some() || !args.clipboard;
        let png = if save { img.encode_png()? } else { img.encode_png_quick()? };
        let path = if save { write_png(&cfg, args.output.as_deref(), &png)? } else { None };
        if let Some(p) = &path
            && args.output.is_none()
        {
            println!("{}", p.display());
        }
        if args.clipboard {
            clipboard::copy(&img, &png)?;
            if cfg.notify {
                tray::notify("Screenshot copied", &format!("{} × {}", img.width, img.height), None);
            }
        } else if let (true, Some(p)) = (cfg.notify && args.output.is_none(), &path) {
            tray::notify("Screenshot saved", &p.display().to_string(), Some(p));
        }
        return Ok(());
    }

    // Fonts and the window list come while the screens are captured.
    let font_path = cfg.font.clone();
    let fonts = std::thread::spawn(move || crate::text::Fonts::load(font_path.as_deref()));
    let scale = desk.scale;
    let wins = std::thread::spawn(move || windows::list(scale));
    let shots = desk.capture(&(0..desk.outputs.len()).collect::<Vec<_>>(), cursor)?;
    let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
    let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
    ed.windows = wins.join().unwrap_or_default();
    ed.palette = cfg.palette_rgb();
    // It can switch to recording the selection (Ctrl+R, or its buttons).
    ed.can_switch = true;
    ed.gif = crate::wants_gif(&cfg, &args);
    let audio = Source::parse(args.audio.as_deref().unwrap_or(&cfg.record_audio)).unwrap_or_default();
    let (system, mic) = Source::toggles(&audio);
    ed.sound = crate::editor::SoundToggles { system, mic };

    let mut session = overlay::Session::new(ed, &desk)?;
    let outcome = session.run()?;
    crate::remember_gif(&cfg, &args, session.editor());
    if outcome == Outcome::Record {
        let picked = recording_picked(session)?;
        crate::record::ensure_idle()?;
        return record(&cfg, &args, picked, audio);
    }
    if session.editor().palette_changed
        && let Err(e) = Config::save_palette(&session.editor().palette)
    {
        eprintln!("clipt: saving the palette: {e:#}");
    }
    if outcome == Outcome::Cancel {
        return Ok(());
    }
    let img = session.editor().export().context("nothing selected")?;
    session.hide();
    drop(session);
    let save = outcome == Outcome::Save || args.output.is_some();
    let png = if save { img.encode_png()? } else { img.encode_png_quick()? };
    let want_copy = outcome == Outcome::Copy || cfg.copy_on_save;
    if want_copy {
        clipboard::copy(&img, &png)?;
    }
    let saved = if save { write_png(&cfg, args.output.as_deref(), &png)? } else { None };
    if cfg.notify {
        match (&saved, want_copy) {
            (Some(p), true) => tray::notify("Screenshot saved and copied", &p.display().to_string(), Some(p)),
            (Some(p), false) => tray::notify("Screenshot saved", &p.display().to_string(), Some(p)),
            (None, true) => tray::notify("Screenshot copied", &format!("{} × {}", img.width, img.height), None),
            _ => {}
        }
    }
    Ok(())
}

/// The overlay's choice of what to record: the window or area, the sound
/// toggles, and whether to make a GIF.
type Picked = (Target, crate::editor::SoundToggles, bool);

/// `clipt record`, and `--stop`, `--pause` and friends.
fn record_cmd(mut args: Args, cfg: &Config) -> Result<()> {
    if args.backend.is_some() {
        bail!("--backend: Windows has only the one");
    }
    if args.stop {
        return crate::record::stop();
    }
    if let Some(cmd) = args.pause.take() {
        return crate::record::pause(cmd);
    }
    crate::record::ensure_idle()?;
    // Before the overlay: a bad setting stops here.
    let audio = Source::parse(args.audio.as_deref().unwrap_or(&cfg.record_audio))?;
    crate::record::Balance::parse(args.audio_balance.as_deref().unwrap_or(&cfg.record_audio_balance))?;
    if !audio.is_empty() {
        crate::record::audio::check(&audio)?;
    }
    capture::dpi_aware();
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
    }
    let (system, mic) = Source::toggles(&audio);
    let sound = crate::editor::SoundToggles { system, mic };
    let gif = crate::wants_gif(cfg, &args);
    let picked = match (args.record_mode.as_deref(), &args.geometry) {
        (_, Some(g)) => (Target::Geometry(g.clone()), sound, gif),
        (Some("screen"), None) => (Target::Screen(args.screen.clone()), sound, gif),
        (Some("window"), None) if args.screen.is_some() => {
            let app = args.screen.as_deref().unwrap_or_default();
            let desk = capture::Desktop::get()?;
            let w = windows::list(desk.scale)
                .into_iter()
                .find(|w| w.app.eq_ignore_ascii_case(app) || w.app.eq_ignore_ascii_case(&format!("{app}.exe")))
                .with_context(|| format!("no window of the app '{app}'"))?;
            (Target::Window(w), sound, gif)
        }
        (None | Some("region" | "window"), None) => {
            let desk = capture::Desktop::get()?;
            let cursor = args.cursor || cfg.cursor;
            let font_path = cfg.font.clone();
            let fonts = std::thread::spawn(move || crate::text::Fonts::load(font_path.as_deref()));
            let scale = desk.scale;
            let wins = std::thread::spawn(move || windows::list(scale));
            let shots = desk.capture(&(0..desk.outputs.len()).collect::<Vec<_>>(), cursor)?;
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
            ed.record = true;
            ed.sound = sound;
            ed.gif = gif;
            ed.windows = wins.join().unwrap_or_default();
            let mut session = overlay::Session::new(ed, &desk)?;
            let outcome = session.run()?;
            crate::remember_gif(cfg, &args, session.editor());
            if outcome != Outcome::Record {
                return Ok(()); // cancelled
            }
            recording_picked(session)?
        }
        (Some(m), None) => bail!("unknown recording mode '{m}' (screen, region, window, or -g GEOM)"),
    };
    record(cfg, &args, picked, audio)
}

/// What a record-mode overlay picked; takes the overlay down.
fn recording_picked(mut session: overlay::Session) -> Result<Picked> {
    let target = crate::recording_target(session.editor())?;
    let (sound, gif) = (session.editor().sound, session.editor().gif);
    session.hide();
    drop(session);
    // Let DWM take the overlay off the screen before the first frame.
    std::thread::sleep(std::time::Duration::from_millis(150));
    Ok((target, sound, gif))
}

fn record(cfg: &Config, args: &Args, (target, sound, gif): Picked, audio: Vec<Source>) -> Result<()> {
    let audio = Source::with_toggles(audio, sound.system, sound.mic);
    if !audio.is_empty() {
        crate::record::audio::check(&audio)?;
    }
    let opts = crate::record::Options {
        target,
        output: args.output.clone(),
        fps: args.fps,
        cursor: args.cursor || cfg.cursor,
        gif: Some(gif),
        audio,
        separate_audio: args.separate_audio || cfg.record_separate_audio,
        balance: crate::record::Balance::parse(args.audio_balance.as_deref().unwrap_or(&cfg.record_audio_balance))?,
    };
    crate::record::run(cfg, opts)
}

/// Started with a console of its own (double-clicked, or from a shortcut),
/// `gui` and `tray` start again without one and return true, so that the
/// console window goes away at once instead of sitting behind the overlay,
/// or beside the tray for as long as it runs. From a terminal, which
/// shares its console, they run as they are.
fn detach() -> Result<bool> {
    let mut pids = [0u32; 2];
    let n = unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(pids.as_mut_ptr(), 2) };
    if n != 1 || std::env::var_os("CLIPT_DETACHED").is_some() {
        return Ok(false);
    }
    use std::os::windows::process::CommandExt;
    // Started by Explorer, this process may take the foreground; the new
    // one is allowed to as well.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX) };
    std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env("CLIPT_DETACHED", "1")
        .creation_flags(tray::DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting clipt without a console")?;
    Ok(true)
}

/// A null-terminated UTF-16 copy of `s`, for the W functions.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A UTF-16 buffer up to its first null.
fn from_wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}
