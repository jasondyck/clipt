//! Minimal text rendering: one sans-serif face parsed with skrifa, glyph
//! outlines filled as tiny-skia paths. Found through fontconfig's `fc-match`
//! (cached), with fallbacks.

use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, MetadataProvider};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Transform};

/// A loaded font file. Parsing the table directory is cheap, so a `FontRef`
/// is built per call instead of keeping a self-referential struct.
pub struct Face {
    data: Vec<u8>,
}

impl Face {
    fn from_bytes(data: Vec<u8>) -> Option<Self> {
        // .ttc collections: take the first face
        (FontRef::new(&data).is_ok() || FontRef::from_index(&data, 0).is_ok()).then_some(Self { data })
    }
    fn font(&self) -> FontRef<'_> {
        FontRef::new(&self.data).or_else(|_| FontRef::from_index(&self.data, 0)).expect("validated in from_bytes")
    }
}

pub struct Fonts {
    pub regular: Face,
    pub bold: Option<Face>,
}

impl Fonts {
    pub fn load(custom: Option<&str>) -> anyhow::Result<Self> {
        let [reg_path, bold_path] = resolve();
        let read = |p: &Option<String>| -> Option<Face> {
            let candidates = p.iter().cloned().chain(FALLBACKS.iter().map(|s| s.to_string()));
            candidates.filter_map(|p| std::fs::read(p).ok()).find_map(Face::from_bytes)
        };
        let regular = match custom {
            Some(p) => Face::from_bytes(std::fs::read(p)?).ok_or_else(|| anyhow::anyhow!("{p}: not a usable TTF/OTF font"))?,
            None => read(&reg_path).ok_or_else(|| anyhow::anyhow!("no usable font found (install any TTF/OTF sans)"))?,
        };
        let bold = bold_path.as_ref().and_then(|_| read(&bold_path));
        Ok(Self { regular, bold })
    }

    pub fn bold(&self) -> &Face {
        self.bold.as_ref().unwrap_or(&self.regular)
    }
}

/// Regular and bold sans-serif paths. Asking fontconfig means spawning
/// fc-match (~8 ms each), which dominates startup on compositors with fast
/// capture, so the answer is cached until one of the files disappears.
fn resolve() -> [Option<String>; 2] {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::config::home().join(".cache"))
        .join("clipt/fonts");
    if let Ok(s) = std::fs::read_to_string(&cache) {
        let mut lines = s.lines().map(|l| Some(l.to_string()).filter(|l| !l.is_empty()));
        let got = [lines.next().flatten(), lines.next().flatten()];
        if got.iter().flatten().all(|p| std::path::Path::new(p).exists()) && got[0].is_some() {
            return got;
        }
    }
    let fc = |pattern: &'static str| {
        std::thread::spawn(move || {
            std::process::Command::new("fc-match")
                .args(["-f", "%{file}", pattern])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .filter(|p| !p.is_empty())
        })
    };
    let (r, b) = (fc("sans-serif"), fc("sans-serif:weight=200"));
    let got = [r.join().ok().flatten(), b.join().ok().flatten()];
    if got[0].is_some() {
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&cache, format!("{}\n{}\n", got[0].as_deref().unwrap_or(""), got[1].as_deref().unwrap_or("")));
    }
    got
}

const FALLBACKS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/cantarell/Cantarell-VF.otf",
];

/// Width of a single line in pixels.
pub fn measure(face: &Face, size: f32, text: &str) -> f32 {
    let font = face.font();
    let (cmap, gm) = (font.charmap(), font.glyph_metrics(Size::new(size), LocationRef::default()));
    text.chars().map(|c| gm.advance_width(cmap.map(c).unwrap_or_default()).unwrap_or(0.0)).sum()
}

pub fn line_height(face: &Face, size: f32) -> f32 {
    let m = face.font().metrics(Size::new(size), LocationRef::default());
    m.ascent - m.descent + m.leading
}

/// Collects glyph outlines into one path, flipping font y-up to pixel y-down.
struct Pen<'a> {
    pb: &'a mut PathBuilder,
    x: f32,
    y: f32,
}

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(self.x + x, self.y - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(self.x + x, self.y - y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.pb.quad_to(self.x + cx, self.y - cy, self.x + x, self.y - y);
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        self.pb.cubic_to(self.x + c0x, self.y - c0y, self.x + c1x, self.y - c1y, self.x + x, self.y - y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Draw `text` with its top-left at (x, y). Newlines start new lines.
/// `swap_rb` flips red/blue for BGRA targets.
#[allow(clippy::too_many_arguments)] // plain drawing call; a params struct would only add noise
pub fn draw(pm: &mut PixmapMut, face: &Face, size: f32, x: f32, y: f32, text: &str, color: Color, swap_rb: bool) {
    let font = face.font();
    let sz = Size::new(size);
    let (cmap, gm, outlines) = (font.charmap(), font.glyph_metrics(sz, LocationRef::default()), font.outline_glyphs());
    let m = font.metrics(sz, LocationRef::default());
    let lh = m.ascent - m.descent + m.leading;
    let mut pb = PathBuilder::new();
    for (li, line) in text.split('\n').enumerate() {
        let baseline = y + m.ascent + li as f32 * lh;
        let mut caret = x;
        for ch in line.chars() {
            let gid = cmap.map(ch).unwrap_or_default();
            if let Some(g) = outlines.get(gid) {
                let _ = g.draw(DrawSettings::unhinted(sz, LocationRef::default()), &mut Pen { pb: &mut pb, x: caret, y: baseline });
            }
            caret += gm.advance_width(gid).unwrap_or(0.0);
        }
    }
    let Some(path) = pb.finish() else { return };
    let c = if swap_rb { Color::from_rgba(color.blue(), color.green(), color.red(), color.alpha()).unwrap() } else { color };
    let mut paint = Paint::default();
    paint.set_color(c);
    paint.anti_alias = true;
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}
