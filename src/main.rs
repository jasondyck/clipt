//! clipt — a lightweight Wayland screenshot tool.

mod capture;
mod clipboard;
mod config;
mod dbus;
mod editor;
mod image;
mod notify;
mod overlay;
mod session;
mod text;
mod x11;

use anyhow::{Context, Result, bail};
use capture::{Backend, Capturer, Output, Shot};
use config::Config;
use editor::{Editor, Outcome};
use std::io::Write;
use std::path::PathBuf;

const HELP: &str = "\
clipt — screenshot tool for Wayland

USAGE:
    clipt [gui]           Select a region and annotate it (default)
    clipt full            Capture the whole desktop
    clipt screen [NAME]   Capture one output (default: the first)
    clipt list            Show outputs and available capture backends

OPTIONS:
    -o, --output PATH     Write the PNG to PATH ('-' for stdout) instead of the save dir
    -c, --clipboard       Copy to the clipboard (full/screen)
    -g, --geometry GEOM   Crop to \"X,Y WxH\" in logical pixels (slurp format)
    -d, --delay SECS      Wait before capturing
    -b, --backend NAME    Force a backend: ext, wlr, kwin, portal
        --cursor          Include the pointer
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

Config: ~/.config/clipt/config.toml (save_dir, filename, color, width,
copy_on_save, notify, backend, cursor, font).
";

#[derive(Default)]
struct Args {
    cmd: String,
    screen: Option<String>,
    output: Option<String>,
    clipboard: bool,
    geometry: Option<String>,
    delay: f64,
    backend: Option<String>,
    cursor: bool,
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
            Value(v) if positional == 1 && a.cmd == "screen" => {
                a.screen = Some(v.string()?);
                positional += 1;
            }
            _ => return Err(arg.unexpected().into()),
        }
    }
    Ok(a)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("clipt: {e:#}");
        std::process::exit(1);
    }
}

/// Process start, for CLIPT_DEBUG timings.
pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn run() -> Result<()> {
    let t0 = *START.get_or_init(std::time::Instant::now);
    let dbg = std::env::var_os("CLIPT_DEBUG").is_some();
    let lap = |what: &str| {
        if dbg {
            eprintln!("clipt: {:>7.1} ms  {what}", t0.elapsed().as_secs_f64() * 1e3)
        }
    };
    let args = parse_args()?;
    let cfg = Config::load()?;
    let backend = match args.backend.as_deref().unwrap_or(&cfg.backend) {
        "auto" => None,
        b => Some(Backend::parse(b)?),
    };
    let cursor = args.cursor || cfg.cursor;
    if args.delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(args.delay));
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
        "gui" => {
            lap("connected");
            let (_, shots) = cap.capture(&outputs, cursor, backend)?;
            lap("captured");
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            lap("fonts");
            let kind = cap.kind();
            drop(cap);
            gui(&cfg, &args, shots, fonts, kind, &lap)
        }
        "debug-render" => {
            let (_, shots) = cap.capture(&outputs, cursor, backend)?;
            let fonts = fonts.join().map_err(|_| anyhow::anyhow!("font loader panicked"))??;
            let ed = Editor::new(shots, fonts, false, cfg.color.saturating_sub(1), cfg.width);
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

fn gui(cfg: &Config, args: &Args, shots: Vec<Shot>, fonts: text::Fonts, kind: capture::DisplayKind, lap: &dyn Fn(&str)) -> Result<()> {
    let ed = Editor::new(shots, fonts, true, cfg.color.saturating_sub(1), cfg.width);
    lap("editor");
    let mut session = session::open(ed, kind)?;
    lap("surfaces");
    let outcome = session.run()?;
    if outcome == Outcome::Cancel {
        return Ok(());
    }
    let img = session.editor().export().context("nothing selected")?;

    // Claim the clipboard while we still hold focus, close the overlay right
    // away, and only then encode: the encode can take ~100 ms and the screen
    // shouldn't stay frozen for it. Early paste requests wait for the PNG.
    let want_copy = outcome == Outcome::Copy || cfg.copy_on_save;
    let clip = if want_copy { session.claim_clipboard().map_err(|e| eprintln!("clipt: {e:#}")).ok() } else { None };
    session.hide()?;
    let png = img.encode_png()?;
    if let Some(c) = &clip {
        c.set(png.clone());
    }

    let saved = if outcome == Outcome::Save || args.output.is_some() { Some(write_png(cfg, args.output.as_deref(), &png)?) } else { None };
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

fn deliver(cfg: &Config, args: &Args, img: image::Image, kind: capture::DisplayKind) -> Result<()> {
    let png = img.encode_png()?;
    let (iw, ih) = (img.width, img.height);
    drop(img);
    let path = if args.output.is_some() || !args.clipboard { write_png(cfg, args.output.as_deref(), &png)? } else { None };
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
        match kind {
            capture::DisplayKind::Wayland => clipboard::copy_and_serve(png, daemonize)?,
            capture::DisplayKind::X11 => x11::clip::copy_and_serve(png, daemonize)?,
        }
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
pub fn trim_heap() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Fork into the background: the parent exits immediately, the child carries
/// on (with the Wayland connection it inherited) to serve the clipboard.
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
