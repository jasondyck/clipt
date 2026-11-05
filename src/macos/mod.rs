//! clipt on macOS: screenshots, with the same editor as on Linux,
//! recording (without sound so far), the menu bar icon, and `clipt update`
//! outside clipt.app. See docs/macos-plan.md.

mod capture;
mod clipboard;
mod overlay;
pub(crate) mod record;
mod tray;
mod windows;

use crate::config::Config;
use crate::editor::{Editor, Outcome};
use crate::{Args, crop_geometry, parse_geometry, stitch, write_png};
use anyhow::{Context, Result, bail};
use objc2::MainThreadMarker;

pub const HELP: &str = "\
clipt — screenshot tool

USAGE:
    clipt [gui]           Select a region and annotate it (default)
    clipt full            Capture the whole desktop
    clipt screen [NAME]   Capture one display (default: the main one)
    clipt list            Show the displays
    clipt record          Pick an area or window to record (Enter starts), saved as MP4
    clipt record screen [NAME]  Record one display (default: the main one)
    clipt record window APP     Record an app's frontmost window
    clipt record -g GEOM  Record a region (within one display)
    clipt record --stop   Stop the running recording
    clipt record --pause / --resume / --toggle-pause
    clipt tray            Menu bar icon, with the shortcuts (what opening clipt.app does)
    clipt config [KEY [VALUE]]  Show the settings, one setting, or set it
    clipt update          Update to the latest release (--check: only check)

OPTIONS:
    -o, --output PATH     Write the PNG to PATH ('-' for stdout) instead of the save dir
                          (record: the .mp4/.mkv/.mov/.gif file)
    -c, --clipboard       Copy to the clipboard (full/screen)
    -g, --geometry GEOM   Crop to \"X,Y WxH\" in points (full)
    -d, --delay SECS      Wait before capturing
        --cursor          Include the pointer
        --fps N           Recording frame rate (default: record_fps, 30)
        --gif             Save the recording as a GIF (at most 15 fps)
    -h, --help            Show this help
    -V, --version         Show the version

EDITOR KEYS:
    drag                  select (click = whole screen; drag handles to resize)
    V R E A L P M T X N   select, rect, ellipse, arrow, line, pen, highlighter,
                          text, pixelate, counter
    1-8                   colour          scroll / [ ]   stroke width
    Shift                 square / 45°    arrows         nudge (Shift resizes)
    Return, ⌘C, dbl-click copy            ⌘S             save
    ⌘Z / ⌘⇧Z              undo / redo     Esc / right-click  cancel / back

Config: ~/.config/clipt/config.toml (save_dir, filename, color, palette, width,
copy_on_save, notify, cursor, font, hotkey, hotkey_full, video_dir,
record_fps, record_codec, record_format, max_duration). hotkey is the
tray's shortcut for a region, default \"cmd+shift+2\"; \"\" turns it off.
clipt needs the Screen Recording permission (System Settings > Privacy &
Security) for the app it runs from. Recording needs ffmpeg (brew install ffmpeg)
and macOS 12.3.
";

pub fn run(args: Args, cfg: Config) -> Result<()> {
    // Opened as clipt.app (Finder, Spotlight, Login Items), with nothing on
    // the command line: the menu bar icon.
    let opened_as_app = std::env::args_os().len() == 1 && objc2_foundation::NSBundle::mainBundle().bundleIdentifier().is_some();
    if args.cmd == "tray" || opened_as_app {
        return tray::run(&cfg);
    }
    if args.cmd == "record" {
        return record_cmd(args, &cfg);
    }
    match args.cmd.as_str() {
        "gui" | "full" | "screen" | "list" => {}
        "update" => return crate::update::cli(args.check),
        "gnome-extension" => bail!("`clipt {}` isn't available on macOS yet", args.cmd),
        other => bail!("unknown command '{other}' (try --help)"),
    }
    if args.backend.is_some() {
        bail!("--backend: macOS has only the one");
    }
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let cursor = args.cursor || cfg.cursor;
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
    }
    let outputs = capture::outputs(mtm)?;
    anyhow::ensure!(!outputs.is_empty(), "no displays found");
    if args.cmd == "list" {
        for o in &outputs {
            println!("{}\t{}x{}+{}+{}\t{}", o.name, o.width, o.height, o.x, o.y, o.description);
        }
        return Ok(());
    }
    capture::ensure_permission()?;

    if args.cmd != "gui" {
        let targets = if args.cmd == "screen" {
            let o = match &args.screen {
                Some(n) => outputs.iter().find(|o| &o.name == n).with_context(|| format!("no display named '{n}' (see clipt list)"))?,
                None => &outputs[0],
            };
            vec![o.clone()]
        } else if let Some(g) = &args.geometry {
            let (x, y, w, h) = parse_geometry(g)?;
            let hit: Vec<_> = outputs
                .iter()
                .filter(|o| {
                    (o.x as f64) < x + w && x < (o.x + o.width as i32) as f64 && (o.y as f64) < y + h && y < (o.y + o.height as i32) as f64
                })
                .cloned()
                .collect();
            anyhow::ensure!(!hit.is_empty(), "geometry '{g}' is outside every display");
            hit
        } else {
            outputs.clone()
        };
        let shots = capture::capture(&targets, cursor)?;
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
            clipboard::copy_png(&png)?;
            if cfg.notify {
                notify("Screenshot copied", &format!("{} × {}", img.width, img.height));
            }
        } else if let (true, Some(p)) = (cfg.notify && args.output.is_none(), &path) {
            notify("Screenshot saved", &p.display().to_string());
        }
        return Ok(());
    }

    // Fonts and the window list come while the screens are captured.
    let font_path = cfg.font.clone();
    let fonts = std::thread::spawn(move || crate::text::Fonts::load(font_path.as_deref()));
    let wins = std::thread::spawn(windows::list);
    let shots = capture::capture(&outputs, cursor)?;
    let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
    let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
    ed.windows = wins.join().unwrap_or_default();
    ed.palette = cfg.palette_rgb();

    let mut session = overlay::Session::new(ed, mtm)?;
    let outcome = session.run()?;
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
        clipboard::copy_png(&png)?;
    }
    let saved = if save { write_png(&cfg, args.output.as_deref(), &png)? } else { None };
    if cfg.notify {
        match (&saved, want_copy) {
            (Some(p), true) => notify("Screenshot saved and copied", &p.display().to_string()),
            (Some(p), false) => notify("Screenshot saved", &p.display().to_string()),
            (None, true) => notify("Screenshot copied", &format!("{} × {}", img.width, img.height)),
            _ => {}
        }
    }
    Ok(())
}

/// `clipt record`: stop or pause the running recording, or start one.
fn record_cmd(mut args: Args, cfg: &Config) -> Result<()> {
    use crate::record::{self, Target};
    if args.stop {
        return record::stop();
    }
    if let Some(cmd) = args.pause.take() {
        return record::pause(cmd);
    }
    if args.audio.as_deref().is_some_and(|a| a != "none") || args.separate_audio || args.audio_balance.is_some() {
        bail!("recording sound isn't available on macOS yet");
    }
    if args.backend.is_some() {
        bail!("--backend: macOS has only the one");
    }
    record::ensure_idle()?;
    let mtm = MainThreadMarker::new().context("not on the main thread")?;
    let cursor = args.cursor || cfg.cursor;
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
    }
    let mut gif = args.gif.then_some(true);
    let target = match (args.record_mode.as_deref(), &args.geometry) {
        (_, Some(g)) => Target::Geometry(g.clone()),
        (Some("screen"), None) => Target::Screen(args.screen.clone()),
        (Some("window"), None) if args.screen.is_some() => {
            let app = args.screen.as_deref().unwrap_or_default();
            // The list is front to back, so this is the app's frontmost window.
            let w = windows::list()
                .into_iter()
                .find(|w| w.app.eq_ignore_ascii_case(app))
                .with_context(|| format!("no window of an app named '{app}'"))?;
            Target::Window(w)
        }
        (None | Some("region" | "window"), None) => match pick_recording_area(cfg, &args, cursor, mtm)? {
            Some((t, g)) => {
                gif = Some(g);
                t
            }
            None => return Ok(()), // cancelled
        },
        (Some(m), None) => bail!("unknown recording mode '{m}' (screen, region, window, or -g GEOM)"),
    };
    let opts = record::Options {
        target,
        output: args.output.clone(),
        fps: args.fps,
        cursor,
        gif,
        audio: Vec::new(),
        separate_audio: false,
        balance: record::Balance::default(),
    };
    record::run(cfg, opts)
}

/// The overlay in record mode: the area dragged or the window clicked, and
/// whether to save a GIF; `None` if cancelled.
fn pick_recording_area(cfg: &Config, args: &Args, cursor: bool, mtm: MainThreadMarker) -> Result<Option<(crate::record::Target, bool)>> {
    capture::ensure_permission()?;
    let font_path = cfg.font.clone();
    let fonts = std::thread::spawn(move || crate::text::Fonts::load(font_path.as_deref()));
    let wins = std::thread::spawn(windows::list);
    let outputs = capture::outputs(mtm)?;
    anyhow::ensure!(!outputs.is_empty(), "no displays found");
    let shots = capture::capture(&outputs, cursor)?;
    let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
    let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
    ed.record = true;
    ed.can_sound = false;
    ed.gif = crate::wants_gif(cfg, args);
    ed.windows = wins.join().unwrap_or_default();
    let mut session = overlay::Session::new(ed, mtm)?;
    let outcome = session.run()?;
    crate::remember_gif(cfg, args, session.editor());
    if outcome != Outcome::Record {
        return Ok(None);
    }
    let ed = session.editor();
    let gif = ed.gif;
    let target = crate::recording_target(ed)?;
    // Before the first frame, and before the recording keeps this thread.
    session.dismiss();
    Ok(Some((target, gif)))
}

/// A notification through AppleScript, which needs no app bundle (it shows
/// as Script Editor's). The text goes in as arguments, not as script.
pub(crate) fn notify(summary: &str, body: &str) {
    let r = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title \"clipt\" subtitle (item 1 of argv)",
            "-e",
            "end run",
        ])
        .args([summary, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match r {
        // Reaped in the background: in the long-running tray it would
        // otherwise linger as a zombie.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => eprintln!("clipt: notification failed: {e}"),
    }
}
