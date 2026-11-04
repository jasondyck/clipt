//! clipt on macOS: screenshots, with the same editor as on Linux, and the
//! menu bar icon. Recording and `clipt update` aren't there yet; see
//! docs/macos-plan.md.

mod capture;
mod clipboard;
mod overlay;
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
    clipt tray            Menu bar icon, with the shortcuts (what opening clipt.app does)
    clipt config [KEY [VALUE]]  Show the settings, one setting, or set it

OPTIONS:
    -o, --output PATH     Write the PNG to PATH ('-' for stdout) instead of the save dir
    -c, --clipboard       Copy to the clipboard (full/screen)
    -g, --geometry GEOM   Crop to \"X,Y WxH\" in points (full)
    -d, --delay SECS      Wait before capturing
        --cursor          Include the pointer
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
copy_on_save, notify, cursor, font, hotkey, hotkey_full). hotkey is the
tray's shortcut for a region, default \"cmd+shift+2\"; \"\" turns it off.
clipt needs the Screen Recording permission (System Settings > Privacy &
Security) for the app it runs from.
";

/// The browser extension's native messaging host (see `crate::browser`).
/// The pasteboard keeps its data itself, so nothing lingers.
pub fn browser_host(cfg: &Config) -> Result<()> {
    let copy = |png: Vec<u8>, ready: &mut dyn FnMut()| -> Result<()> {
        clipboard::copy_png(&png)?;
        ready();
        Ok(())
    };
    crate::browser::serve(cfg, copy, |_, _, _| {})
}

pub fn run(args: Args, cfg: Config) -> Result<()> {
    // Opened as clipt.app (Finder, Spotlight, Login Items), with nothing on
    // the command line: the menu bar icon.
    let opened_as_app = std::env::args_os().len() == 1 && objc2_foundation::NSBundle::mainBundle().bundleIdentifier().is_some();
    if args.cmd == "tray" || opened_as_app {
        return tray::run(&cfg);
    }
    match args.cmd.as_str() {
        "gui" | "full" | "screen" | "list" => {}
        "browser-extension" => return crate::browser::install(),
        "record" | "update" | "gnome-extension" => bail!("`clipt {}` isn't available on macOS yet", args.cmd),
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
        let png = img.encode_png()?;
        let path = if args.output.is_some() || !args.clipboard { write_png(&cfg, args.output.as_deref(), &png)? } else { None };
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
    let png = img.encode_png()?;
    let want_copy = outcome == Outcome::Copy || cfg.copy_on_save;
    if want_copy {
        clipboard::copy_png(&png)?;
    }
    let saved = if outcome == Outcome::Save || args.output.is_some() { write_png(&cfg, args.output.as_deref(), &png)? } else { None };
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

/// A notification through AppleScript, which needs no app bundle (it shows
/// as Script Editor's). The text goes in as arguments, not as script.
fn notify(summary: &str, body: &str) {
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
    if let Err(e) = r {
        eprintln!("clipt: notification failed: {e}");
    }
}
