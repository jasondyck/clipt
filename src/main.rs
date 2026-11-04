//! clipt — a lightweight screenshot tool for Wayland, X11, macOS and Windows.

// Some shared code (image rotation, video paths) only serves what macOS
// and Windows can't do yet, recording among it.
#![cfg_attr(not(wayland_x11), allow(dead_code))]

mod browser;
mod capture;
#[cfg(wayland_x11)]
mod clipboard;
mod config;
#[cfg(wayland_x11)]
mod dbus;
mod editor;
mod icon;
mod image;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(wayland_x11)]
mod notify;
#[cfg(wayland_x11)]
mod overlay;
#[cfg(wayland_x11)]
mod record;
#[cfg(wayland_x11)]
mod session;
mod text;
#[cfg(wayland_x11)]
mod tray;
#[cfg(wayland_x11)]
mod update;
#[cfg(windows)]
mod win32;
mod windows;
#[cfg(wayland_x11)]
mod x11;

#[cfg(wayland_x11)]
use anyhow::bail;
use anyhow::{Context, Result};
use capture::Shot;
#[cfg(wayland_x11)]
use capture::{Backend, Capturer, Output};
use config::Config;
#[cfg(wayland_x11)]
use editor::{Editor, Outcome};
use std::io::Write;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
use macos::HELP;
#[cfg(windows)]
use win32::HELP;

#[cfg(wayland_x11)]
const HELP: &str = "\
clipt — screenshot tool for Wayland and X11

USAGE:
    clipt [gui]           Select a region and annotate it (default)
    clipt full            Capture the whole desktop
    clipt screen [NAME]   Capture one output (default: the first)
    clipt list            Show outputs and available capture backends
    clipt list sound      Show the sound devices recordings can use
    clipt record          Pick an area to record (Enter starts), saved as MP4
    clipt record screen [NAME]  Record one output (default: the first)
    clipt record window [APP]   Record a window (APP: the topmost with that app id)
    clipt record -g GEOM  Record a region (within one output)
    clipt record --stop   Stop the running recording
    clipt record --pause / --resume / --toggle-pause
    clipt tray            Sit in the system tray (click: region, right-click: menu)
    clipt update          Update to the latest release (--check: only check)
    clipt config [KEY [VALUE]]  Show the settings, one setting, or set it,
                          e.g. clipt config record_format gif
    clipt gnome-extension Install the GNOME Shell extension that lets the
                          overlay highlight windows on GNOME
    clipt browser-extension  Set up the Chrome/Edge extension that opens a
                          whole web page in the editor
    clipt edit FILE       Open a PNG in the editor (- reads standard input)

OPTIONS:
    -o, --output PATH     Write the PNG to PATH ('-' for stdout) instead of the save dir
                          (record: the .mp4/.mkv/.mov/.gif file)
    -c, --clipboard       Copy to the clipboard (full/screen)
    -g, --geometry GEOM   Crop to \"X,Y WxH\" in logical pixels (slurp format)
    -d, --delay SECS      Wait before capturing
    -b, --backend NAME    Force a backend: ext, wlr, kwin, mutter, portal, x11
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
                          clearly the voice came through). Default: off;
                          needs the microphone and at least one other source
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

Config: ~/.config/clipt/config.toml (save_dir, filename, color, palette, width,
copy_on_save, notify, backend, cursor, font, video_dir, video_filename,
record_fps, record_codec, record_format, record_audio,
record_audio_balance, record_separate_audio, max_duration).
Recording needs ffmpeg; on KDE and GNOME also PipeWire; with sound, a
PulseAudio or pipewire-pulse server.
";

#[derive(Default)]
// macOS doesn't record yet, so it reads fewer of these.
#[cfg_attr(not(wayland_x11), allow(dead_code))]
struct Args {
    cmd: String,
    screen: Option<String>,
    output: Option<String>,
    clipboard: bool,
    geometry: Option<String>,
    delay: f64,
    backend: Option<String>,
    cursor: bool,
    check: bool,
    record_mode: Option<String>,
    fps: Option<u32>,
    stop: bool,
    #[cfg(wayland_x11)]
    pause: Option<record::PauseCmd>,
    gif: bool,
    /// --audio[=SOURCES] or --no-audio ("none").
    audio: Option<String>,
    /// --audio-balance=MODE.
    audio_balance: Option<String>,
    separate_audio: bool,
    /// `clipt config [KEY [VALUE]]`.
    config: Vec<String>,
}

fn parse_args() -> Result<Args> {
    use lexopt::prelude::*;
    let mut a = Args { cmd: "gui".into(), ..Default::default() };
    let mut p = lexopt::Parser::from_env();
    let mut positional = 0;
    while let Some(arg) = p.next()? {
        match arg {
            Short('o') | Long("output") => a.output = Some(p.value()?.string()?),
            Short('c') | Long("clipboard") => a.clipboard = true,
            Short('g') | Long("geometry") => a.geometry = Some(p.value()?.string()?),
            Short('d') | Long("delay") => a.delay = p.value()?.parse()?,
            Short('b') | Long("backend") => a.backend = Some(p.value()?.string()?),
            Long("cursor") => a.cursor = true,
            Long("check") => a.check = true,
            Long("fps") => a.fps = Some(p.value()?.parse()?),
            Long("gif") => a.gif = true,
            Long("audio") => a.audio = Some(p.optional_value().map(|v| v.string()).transpose()?.unwrap_or_else(|| "system".into())),
            Long("audio-balance") => a.audio_balance = Some(p.value()?.string()?),
            Long("no-audio") => a.audio = Some("none".into()),
            Long("separate-audio") => a.separate_audio = true,
            Long("stop") => a.stop = true,
            #[cfg(wayland_x11)]
            Long("pause") => a.pause = Some(record::PauseCmd::Pause),
            #[cfg(wayland_x11)]
            Long("resume") => a.pause = Some(record::PauseCmd::Resume),
            #[cfg(wayland_x11)]
            Long("toggle-pause") => a.pause = Some(record::PauseCmd::Toggle),
            Short('h') | Long("help") => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Short('V') | Long("version") => {
                println!("clipt {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Value(v) if positional == 0 => {
                a.cmd = v.string()?;
                positional += 1;
            }
            Value(v) if positional == 1 && a.cmd == "list" => {
                a.screen = Some(v.string()?);
                positional += 1;
            }
            Value(v) if positional == 1 && a.cmd == "edit" => {
                a.screen = Some(v.string()?);
                positional += 1;
            }
            Value(v) if positional == 1 && a.cmd == "screen" => {
                a.screen = Some(v.string()?);
                positional += 1;
            }
            Value(v) if positional == 1 && a.cmd == "record" => {
                a.record_mode = Some(v.string()?);
                positional += 1;
            }
            Value(v) if (1..=2).contains(&positional) && a.cmd == "config" => {
                a.config.push(v.string()?);
                positional += 1;
            }
            Value(v) if positional == 2 && matches!(a.record_mode.as_deref(), Some("screen" | "window")) => {
                a.screen = Some(v.string()?);
                positional += 1;
            }
            _ => return Err(arg.unexpected().into()),
        }
    }
    Ok(a)
}

/// `clipt config`: every setting; `clipt config KEY`: one; `clipt config
/// KEY VALUE`: set it in the config file.
fn config_cmd(args: &Args) -> Result<()> {
    match args.config.as_slice() {
        [] => {
            println!("# {}", Config::path().display());
            print!("{}", Config::load()?.listing());
        }
        [key] => {
            let cfg = Config::load()?;
            match cfg.get(key) {
                Some(v) => println!("{v}"),
                None if key == "font" => println!("(not set)"),
                None => anyhow::bail!("no setting '{key}'; `clipt config` lists them"),
            }
        }
        [key, value] => {
            Config::set(key, value)?;
            println!("{key} = {}", Config::load()?.get(key).unwrap_or_default());
        }
        _ => unreachable!("parse_args takes at most two"),
    }
    Ok(())
}

fn main() {
    let r = if browser::started_by_browser() { browser_host() } else { run() };
    if let Err(e) = r {
        eprintln!("clipt: {e:#}");
        std::process::exit(1);
    }
}

/// Process start, for CLIPT_DEBUG timings.
pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Started by Chrome or Edge as the extension's native messaging host: copy
/// or save the page it sends. On Wayland and X11 clipt then stays in the
/// background to serve the paste, as after a screenshot.
#[cfg(wayland_x11)]
fn browser_host() -> Result<()> {
    let cfg = Config::load()?;
    let copy = |png: Vec<u8>, ready: &mut dyn FnMut()| -> Result<()> {
        trim_heap();
        let serving = || {
            ready();
            daemonize();
        };
        copy_and_serve(png, std::env::var_os("WAYLAND_DISPLAY").is_some(), serving)
    };
    browser::serve(&cfg, copy, |summary, body, path| {
        if cfg.notify {
            notify::send(summary, body, path);
        }
    })
}

#[cfg(target_os = "macos")]
fn browser_host() -> Result<()> {
    macos::browser_host(&Config::load()?)
}

#[cfg(windows)]
fn browser_host() -> Result<()> {
    win32::browser_host(&Config::load()?)
}

#[cfg(target_os = "macos")]
fn run() -> Result<()> {
    START.get_or_init(std::time::Instant::now);
    let args = parse_args()?;
    if args.cmd == "debug-icon" {
        return debug_icon(&args);
    }
    if args.cmd == "config" {
        return config_cmd(&args);
    }
    macos::run(args, Config::load()?)
}

#[cfg(windows)]
fn run() -> Result<()> {
    START.get_or_init(std::time::Instant::now);
    let args = parse_args()?;
    if args.cmd == "debug-icon" {
        return debug_icon(&args);
    }
    if args.cmd == "config" {
        return config_cmd(&args);
    }
    win32::run(args, Config::load()?)
}

/// `clipt debug-icon [-o PATH]`: a contact sheet of the icon's sizes, or,
/// for a PATH ending in .iconset, the macOS app icon's PNGs, which
/// packaging/macos turns into an .icns with `iconutil`.
fn debug_icon(args: &Args) -> Result<()> {
    let out = args.output.as_deref().unwrap_or("clipt-icon-preview.png");
    if out.ends_with(".iconset") {
        return icon::iconset(std::path::Path::new(out));
    }
    std::fs::write(out, icon::preview().encode_png()?)?;
    Ok(())
}

#[cfg(wayland_x11)]
fn run() -> Result<()> {
    let t0 = *START.get_or_init(std::time::Instant::now);
    let dbg = std::env::var_os("CLIPT_DEBUG").is_some();
    let lap = |what: &str| {
        if dbg {
            eprintln!("clipt: {:>7.1} ms  {what}", t0.elapsed().as_secs_f64() * 1e3)
        }
    };
    let mut args = parse_args()?;
    if args.cmd == "config" {
        return config_cmd(&args);
    }
    let cfg = Config::load()?;
    let backend = match args.backend.as_deref().unwrap_or(&cfg.backend) {
        "auto" => None,
        b => Some(Backend::parse(b)?),
    };
    if args.cmd == "debug-icon" {
        return debug_icon(&args);
    }
    if args.cmd == "tray" {
        return tray::run(
            config::expand(&cfg.save_dir).display().to_string(),
            config::expand(&cfg.video_dir).display().to_string(),
            cfg.check_updates,
            record::audio::Source::parse(&cfg.record_audio).is_ok_and(|s| !s.is_empty()),
        );
    }
    if args.cmd == "update" {
        return update::cli(args.check);
    }
    if args.cmd == "gnome-extension" {
        return windows::install_gnome_extension();
    }
    if args.cmd == "browser-extension" {
        return browser::install();
    }
    if args.cmd == "record" && args.stop {
        return record::stop();
    }
    if args.cmd == "record"
        && let Some(cmd) = args.pause.take()
    {
        return record::pause(cmd);
    }
    if args.cmd == "list" {
        match args.screen.as_deref() {
            Some("sound") => return list_sound(),
            Some(other) => bail!("unknown list '{other}' (clipt list, or clipt list sound)"),
            None => {}
        }
    }
    let mut audio = Vec::new();
    let mut balance = record::Balance::default();
    if args.cmd == "record" {
        record::ensure_idle()?;
        // Before the overlay: a bad setting or missing library stops here.
        audio = record::audio::Source::parse(args.audio.as_deref().unwrap_or(&cfg.record_audio))?;
        balance = record::Balance::parse(args.audio_balance.as_deref().unwrap_or(&cfg.record_audio_balance))?;
        if !audio.is_empty() {
            record::audio::check(&audio)?;
        }
    }
    let cursor = args.cursor || cfg.cursor;
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
    }
    if args.cmd == "record" {
        let mut gif = args.gif.then_some(true);
        let target = match (args.record_mode.as_deref(), &args.geometry) {
            (_, Some(g)) => record::Target::Geometry(g.clone()),
            (Some("screen"), None) => record::Target::Screen(args.screen.clone()),
            (Some("window"), None) if args.screen.is_some() => {
                let app = args.screen.as_deref().unwrap_or_default();
                let x11 = capture::wayland::connection().is_err();
                let w = windows::list(x11)
                    .into_iter()
                    .find(|w| w.app.eq_ignore_ascii_case(app))
                    .with_context(|| format!("no window with app id '{app}'"))?;
                record::Target::Window(w)
            }
            (None | Some("region" | "window"), None) => {
                let (system, mic) = record::audio::Source::toggles(&audio);
                match pick_recording_area(&cfg, &args, cursor, backend, editor::SoundToggles { system, mic })? {
                    Some((t, s, g)) => {
                        gif = Some(g);
                        let had = !audio.is_empty();
                        audio = record::audio::Source::with_toggles(audio, s.system, s.mic);
                        if !had && !audio.is_empty() {
                            record::audio::check(&audio)?;
                        }
                        t
                    }
                    None => return Ok(()), // cancelled
                }
            }
            (Some(m), None) => bail!("unknown recording mode '{m}' (screen, region, or -g GEOM)"),
        };
        let separate_audio = args.separate_audio || cfg.record_separate_audio;
        let opts =
            record::Options { target, output: args.output.clone(), fps: args.fps, cursor, backend, gif, audio, separate_audio, balance };
        return record::run(&cfg, opts);
    }

    // Font loading overlaps the capture.
    let font_path = cfg.font.clone();
    let fonts = std::thread::spawn(move || text::Fonts::load(font_path.as_deref()));

    let mut cap = Capturer::new()?;
    let outputs = cap.outputs();
    if outputs.is_empty() {
        bail!("no outputs found");
    }

    match args.cmd.as_str() {
        "list" => {
            for o in &outputs {
                println!("{}\t{}x{}+{}+{}\t{}", o.name, o.width, o.height, o.x, o.y, o.description);
            }
            let avail: Vec<String> = cap.available().iter().map(|b| format!("{b:?}").to_lowercase()).collect();
            println!("backends: {}", avail.join(", "));
            Ok(())
        }
        "full" | "screen" => {
            let targets: Vec<Output> = if args.cmd == "screen" {
                let o = match &args.screen {
                    Some(n) => outputs.iter().find(|o| &o.name == n).with_context(|| format!("no output named '{n}'"))?,
                    None => &outputs[0],
                };
                vec![o.clone()]
            } else if let Some(g) = &args.geometry {
                // Like grim: only the outputs the region touches, so the
                // result is at the highest scale among *those*.
                let (x, y, w, h) = parse_geometry(g)?;
                let hit: Vec<Output> = outputs
                    .iter()
                    .filter(|o| {
                        (o.x as f64) < x + w
                            && x < (o.x + o.width as i32) as f64
                            && (o.y as f64) < y + h
                            && y < (o.y + o.height as i32) as f64
                    })
                    .cloned()
                    .collect();
                anyhow::ensure!(!hit.is_empty(), "geometry '{g}' is outside every output");
                hit
            } else {
                outputs.clone()
            };
            let (_, shots) = cap.capture(&targets, cursor, backend)?;
            let kind = cap.kind();
            drop(cap);
            let mut img = stitch(&shots);
            if let Some(g) = &args.geometry {
                img = crop_geometry(&img, &shots, g)?;
            }
            drop(shots);
            deliver(&cfg, &args, img, kind)
        }
        "edit" => {
            let path = args.screen.as_deref().context("which image? clipt edit FILE, or - to read it from standard input")?;
            let bytes = if path == "-" {
                let mut b = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut b).context("reading standard input")?;
                b
            } else {
                let p = config::expand(path);
                std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?
            };
            let img = image::Image::decode_png(&bytes).context("clipt edit opens PNG images")?;
            drop(bytes);
            // One image pixel to one physical pixel on the first display.
            let display = outputs[0].clone();
            let scale = if display.native.0 > 0 { display.native.0 as f32 / display.width as f32 } else { 1.0 };
            let kind = cap.kind();
            drop(cap);
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            let ed = Editor::for_image(&img, display, scale, fonts, cfg.color.saturating_sub(1), cfg.width);
            drop(img);
            gui(&cfg, &args, ed, kind, backend, &lap)
        }
        "gui" => {
            lap("connected");
            // Window positions are fetched while the screen is captured.
            let x11 = cap.kind() == capture::DisplayKind::X11;
            let wins = std::thread::spawn(move || windows::list(x11));
            let (_, shots) = cap.capture(&outputs, cursor, backend)?;
            lap("captured");
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            lap("fonts");
            let kind = cap.kind();
            drop(cap);
            let wins = wins.join().unwrap_or_default();
            lap("windows");
            let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
            ed.windows = wins;
            gui(&cfg, &args, ed, kind, backend, &lap)
        }
        "debug-render" => {
            let (_, shots) = cap.capture(&outputs, cursor, backend)?;
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            let mut ed = Editor::new(shots, fonts, false, cfg.color.saturating_sub(1), cfg.width);
            ed.palette = cfg.palette_rgb();
            ed.windows = windows::list(cap.kind() == capture::DisplayKind::X11);
            // As the screenshot overlay shows it; CLIPT_RECORD=1 in record mode.
            ed.can_switch = true;
            ed.record = std::env::var_os("CLIPT_RECORD").is_some();
            // CLIPT_POINTER=x,y renders the hover state at that point.
            if let Some((x, y)) = std::env::var("CLIPT_POINTER").ok().and_then(|v| {
                let (x, y) = v.split_once(',')?;
                Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
            }) {
                ed.pointer_motion((x, y), Default::default());
            }
            let _ = ed.take_damage();
            let s = &ed.screens[0];
            let mut pm = tiny_skia::Pixmap::new(s.bg.width(), s.bg.height()).unwrap();
            editor::ui::render(&ed, 0, &mut pm.as_mut(), (0, 0));
            let img = image::Image { width: pm.width(), height: pm.height(), data: pm.take() };
            std::fs::write(args.output.as_deref().unwrap_or("/tmp/clipt-ui.png"), img.encode_png()?)?;
            Ok(())
        }
        other => bail!("unknown command '{other}' (try --help)"),
    }
}

#[cfg(wayland_x11)]
fn gui(cfg: &Config, args: &Args, mut ed: Editor, kind: capture::DisplayKind, backend: Option<Backend>, lap: &dyn Fn(&str)) -> Result<()> {
    // It can switch to recording the selection (Ctrl+R, or its buttons),
    // with the sound the settings would record; not when it's an image.
    ed.can_switch = ed.doc.is_none();
    ed.palette = cfg.palette_rgb();
    ed.gif = wants_gif(cfg, args);
    let audio = record::audio::Source::parse(args.audio.as_deref().unwrap_or(&cfg.record_audio)).unwrap_or_default();
    let (system, mic) = record::audio::Source::toggles(&audio);
    ed.sound = editor::SoundToggles { system, mic };
    lap("editor");
    let mut session = session::open(ed, kind)?;
    lap("surfaces");
    let outcome = session.run()?;
    remember_gif(cfg, args, session.editor());
    if session.editor().palette_changed
        && let Err(e) = Config::save_palette(&session.editor().palette)
    {
        eprintln!("clipt: saving the palette: {e:#}");
    }
    if outcome == Outcome::Cancel {
        return Ok(());
    }
    if outcome == Outcome::Record {
        let target = recording_target(session.editor())?;
        let sound = session.editor().sound;
        let gif = session.editor().gif;
        session.hide()?;
        drop(session);
        // Let the compositor take the overlay down before the first frame.
        std::thread::sleep(std::time::Duration::from_millis(150));
        return record_picked(cfg, args, target, audio, sound, gif, backend);
    }
    let img = session.editor().export().context("nothing selected")?;

    // Claim the clipboard while we still hold focus, close the overlay right
    // away, and only then encode: the encode can take ~100 ms and the screen
    // shouldn't stay frozen for it. Early paste requests wait for the PNG.
    let want_copy = outcome == Outcome::Copy || cfg.copy_on_save;
    let clip = if want_copy { session.claim_clipboard().map_err(|e| eprintln!("clipt: {e:#}")).ok() } else { None };
    session.hide()?;
    let save = outcome == Outcome::Save || args.output.is_some();
    let png = if save { img.encode_png()? } else { img.encode_png_quick()? };
    if let Some(c) = &clip {
        c.set(png.clone());
    }

    let saved = if save { Some(write_png(cfg, args.output.as_deref(), &png)?) } else { None };
    if cfg.notify {
        match (&saved, want_copy) {
            (Some(Some(p)), true) => notify::send("Screenshot saved and copied", &p.display().to_string(), Some(p)),
            (Some(Some(p)), false) => notify::send("Screenshot saved", &p.display().to_string(), Some(p)),
            (_, true) => notify::send("Screenshot copied", &format!("{} × {}", img.width, img.height), None),
            _ => {}
        }
    }
    if want_copy {
        // Detach so the launching terminal/hotkey returns while we keep
        // answering paste requests, holding only the PNG.
        drop((img, png));
        session.release_memory();
        daemonize();
        session.serve_clipboard()?;
    }
    Ok(())
}

/// `clipt list sound`: what `--audio` can record, marking what `system` and
/// `mic` stand for.
#[cfg(wayland_x11)]
fn list_sound() -> Result<()> {
    for d in record::audio::devices()? {
        let mark = match (d.is_system, d.is_mic) {
            (true, _) => "\t(system)",
            (_, true) => "\t(mic)",
            _ => "",
        };
        println!("{}\t{}{mark}", d.name, d.description);
    }
    Ok(())
}

/// The overlay in record mode: what to record (an area, or a window picked
/// with a click), and the sound toggles (starting from `sound`) and GIF
/// toggle as the user left them, or None if cancelled.
#[cfg(wayland_x11)]
fn pick_recording_area(
    cfg: &Config,
    args: &Args,
    cursor: bool,
    backend: Option<Backend>,
    sound: editor::SoundToggles,
) -> Result<Option<(record::Target, editor::SoundToggles, bool)>> {
    let font_path = cfg.font.clone();
    let fonts = std::thread::spawn(move || text::Fonts::load(font_path.as_deref()));
    let mut cap = Capturer::new()?;
    let outputs = cap.outputs();
    anyhow::ensure!(!outputs.is_empty(), "no outputs found");
    let x11 = cap.kind() == capture::DisplayKind::X11;
    let wins = std::thread::spawn(move || windows::list(x11));
    let (_, shots) = cap.capture(&outputs, cursor, backend)?;
    let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
    let kind = cap.kind();
    drop(cap);
    let mut ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
    ed.record = true;
    ed.sound = sound;
    ed.gif = wants_gif(cfg, args);
    ed.windows = wins.join().unwrap_or_default();
    let mut session = session::open(ed, kind)?;
    let outcome = session.run()?;
    remember_gif(cfg, args, session.editor());
    if outcome != Outcome::Record {
        return Ok(None);
    }
    let target = recording_target(session.editor())?;
    let sound = session.editor().sound;
    let gif = session.editor().gif;
    session.hide()?;
    drop(session);
    // Let the compositor take the overlay down before the first frame.
    std::thread::sleep(std::time::Duration::from_millis(150));
    Ok(Some((target, sound, gif)))
}

/// Whether a recording starts out as a GIF: `--gif`, or `record_format`.
#[cfg(wayland_x11)]
fn wants_gif(cfg: &Config, args: &Args) -> bool {
    args.gif || cfg.record_format == "gif"
}

/// The overlay's GIF toggle is remembered: when it was flipped, it becomes
/// `record_format`, so the tray, `clipt record` and the next Print Screen
/// follow it.
#[cfg(wayland_x11)]
fn remember_gif(cfg: &Config, args: &Args, ed: &Editor) {
    if ed.gif != wants_gif(cfg, args)
        && let Err(e) = Config::set("record_format", if ed.gif { "gif" } else { "mp4" })
    {
        eprintln!("clipt: saving record_format: {e:#}");
    }
}

/// What a record-mode overlay picked: the window clicked, or the area.
#[cfg(wayland_x11)]
fn recording_target(ed: &Editor) -> Result<record::Target> {
    let r = ed.final_rect().context("nothing selected")?;
    Ok(match ed.selected_window() {
        Some(w) => record::Target::Window(w.clone()),
        None => record::Target::Rect((r.left() as f64, r.top() as f64, r.width() as f64, r.height() as f64)),
    })
}

/// Record what the screenshot overlay was switched to record, with the sound
/// as toggled there, and otherwise as `clipt record` would.
#[cfg(wayland_x11)]
fn record_picked(
    cfg: &Config,
    args: &Args,
    target: record::Target,
    audio: Vec<record::audio::Source>,
    sound: editor::SoundToggles,
    gif: bool,
    backend: Option<Backend>,
) -> Result<()> {
    let cursor = args.cursor || cfg.cursor;
    record::ensure_idle()?;
    let audio = record::audio::Source::with_toggles(audio, sound.system, sound.mic);
    if !audio.is_empty() {
        record::audio::check(&audio)?;
    }
    let balance = record::Balance::parse(args.audio_balance.as_deref().unwrap_or(&cfg.record_audio_balance))?;
    let opts = record::Options {
        target,
        output: None,
        fps: args.fps,
        cursor,
        backend,
        gif: Some(gif),
        audio,
        separate_audio: args.separate_audio || cfg.record_separate_audio,
        balance,
    };
    record::run(cfg, opts)
}

#[cfg(wayland_x11)]
fn deliver(cfg: &Config, args: &Args, img: image::Image, kind: capture::DisplayKind) -> Result<()> {
    let save = args.output.is_some() || !args.clipboard;
    let png = if save { img.encode_png()? } else { img.encode_png_quick()? };
    let (iw, ih) = (img.width, img.height);
    drop(img);
    let path = if save { write_png(cfg, args.output.as_deref(), &png)? } else { None };
    if let Some(p) = &path
        && args.output.is_none()
    {
        println!("{}", p.display());
    }
    if args.clipboard {
        if cfg.notify {
            notify::send("Screenshot copied", &format!("{iw} × {ih}"), path.as_deref());
        }
        // Hand freed pages back before lingering as the clipboard owner.
        crate::trim_heap();
        copy_and_serve(png, kind == capture::DisplayKind::Wayland, daemonize)?;
    } else if let (true, Some(p)) = (cfg.notify && args.output.is_none(), &path) {
        notify::send("Screenshot saved", &p.display().to_string(), Some(p));
    }
    Ok(())
}

/// Returns the path written, or None for stdout.
fn write_png(cfg: &Config, output: Option<&str>, png: &[u8]) -> Result<Option<PathBuf>> {
    match output {
        Some("-") => {
            std::io::stdout().lock().write_all(png)?;
            Ok(None)
        }
        Some(p) => {
            let p = config::expand(p);
            std::fs::write(&p, png).with_context(|| format!("writing {}", p.display()))?;
            Ok(Some(p))
        }
        None => {
            let p = cfg.next_save_path();
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
            }
            std::fs::write(&p, png).with_context(|| format!("writing {}", p.display()))?;
            Ok(Some(p))
        }
    }
}

/// Composite per-output shots into one image in logical layout. Mixed scales
/// are normalised to the largest so nothing is downsampled.
fn stitch(shots: &[Shot]) -> image::Image {
    if shots.len() == 1 {
        return shots[0].image.clone();
    }
    let scale = shots.iter().map(|s| s.scale()).fold(1.0f64, f64::max);
    let minx = shots.iter().map(|s| s.output.x).min().unwrap();
    let miny = shots.iter().map(|s| s.output.y).min().unwrap();
    let maxx = shots.iter().map(|s| s.output.x + s.output.width as i32).max().unwrap();
    let maxy = shots.iter().map(|s| s.output.y + s.output.height as i32).max().unwrap();
    let w = ((maxx - minx) as f64 * scale).round() as u32;
    let h = ((maxy - miny) as f64 * scale).round() as u32;
    let mut pm = tiny_skia::Pixmap::new(w, h).unwrap();
    for s in shots {
        let src =
            tiny_skia::Pixmap::from_vec(s.image.data.clone(), tiny_skia::IntSize::from_wh(s.image.width, s.image.height).unwrap()).unwrap();
        let k = (scale / s.scale()) as f32;
        let tx = ((s.output.x - minx) as f64 * scale).round() as f32;
        let ty = ((s.output.y - miny) as f64 * scale).round() as f32;
        let q = if (k - 1.0).abs() < 1e-3 { tiny_skia::FilterQuality::Nearest } else { tiny_skia::FilterQuality::Bicubic };
        let paint = tiny_skia::PixmapPaint { quality: q, ..Default::default() };
        pm.draw_pixmap(0, 0, src.as_ref(), &paint, tiny_skia::Transform::from_row(k, 0.0, 0.0, k, tx, ty), None);
    }
    image::Image { width: w, height: h, data: pm.take() }
}

/// Parse slurp-style "X,Y WxH" logical geometry.
fn parse_geometry(g: &str) -> Result<(f64, f64, f64, f64)> {
    let bad = || anyhow::anyhow!("bad geometry '{g}', expected \"X,Y WxH\"");
    let (pos, size) = g.trim().split_once(' ').ok_or_else(bad)?;
    let (x, y) = pos.split_once(',').ok_or_else(bad)?;
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let p = |s: &str| s.trim().parse::<f64>().map_err(|_| bad());
    let r = (p(x)?, p(y)?, p(w)?, p(h)?);
    anyhow::ensure!(r.2 > 0.0 && r.3 > 0.0, bad());
    Ok(r)
}

/// Crop a stitched image to slurp-style geometry.
fn crop_geometry(img: &image::Image, shots: &[Shot], g: &str) -> Result<image::Image> {
    let (x, y, w, h) = parse_geometry(g)?;
    let scale = shots.iter().map(|s| s.scale()).fold(1.0f64, f64::max);
    let minx = shots.iter().map(|s| s.output.x).min().unwrap_or(0) as f64;
    let miny = shots.iter().map(|s| s.output.y).min().unwrap_or(0) as f64;
    let r = |v: f64| (v * scale).round().max(0.0) as u32;
    Ok(img.crop(r(x - minx), r(y - miny), r(w).max(1), r(h).max(1)))
}

/// Return freed heap pages to the OS before lingering in the background.
/// glibc keeps them otherwise; musl and the BSD allocators release large
/// frees on their own.
#[cfg(wayland_x11)]
pub fn trim_heap() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Own the clipboard without a window, and serve the PNG until something
/// else is copied. A Wayland compositor without data-control (GNOME) gets
/// the X11 clipboard through XWayland instead, which it passes on to
/// Wayland apps.
#[cfg(wayland_x11)]
fn copy_and_serve(png: Vec<u8>, wayland: bool, before_serving: impl FnOnce()) -> Result<()> {
    if !wayland {
        return x11::clip::copy_and_serve(png, before_serving);
    }
    if clipboard::has_data_control() || std::env::var_os("DISPLAY").is_none() {
        return clipboard::copy_and_serve(png, before_serving);
    }
    x11::clip::copy_and_serve(png, before_serving)
        .context("this compositor has no data-control protocol, and the X11 clipboard through XWayland didn't work either")
}

/// Fork into the background: the parent exits immediately, the child carries
/// on (with the Wayland connection it inherited) to serve the clipboard.
#[cfg(wayland_x11)]
fn daemonize() {
    unsafe {
        match libc::fork() {
            -1 => {} // keep serving in the foreground
            0 => {
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                }
            }
            _ => libc::_exit(0),
        }
    }
}
