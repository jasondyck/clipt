//! Plain RGBA8 image buffer shared by the capture backends and the editor.

/// Straight (non-premultiplied) RGBA, row-major, no padding.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, data: vec![0; width as usize * height as usize * 4] }
    }

    /// Build from rows with an arbitrary stride, converting each pixel with `f`.
    /// `bpp` is the source bytes per pixel.
    pub fn from_rows(width: u32, height: u32, stride: usize, bpp: usize, src: &[u8], f: impl Fn(&[u8]) -> [u8; 4]) -> anyhow::Result<Self> {
        let need = stride * (height as usize - 1) + width as usize * bpp;
        anyhow::ensure!(src.len() >= need, "short image buffer: {} < {}", src.len(), need);
        let mut img = Self::new(width, height);
        for y in 0..height as usize {
            let row = &src[y * stride..y * stride + width as usize * bpp];
            let out = &mut img.data[y * width as usize * 4..(y + 1) * width as usize * 4];
            for (s, d) in row.chunks_exact(bpp).zip(out.chunks_exact_mut(4)) {
                d.copy_from_slice(&f(s));
            }
        }
        Ok(img)
    }

    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Self {
        let x = x.min(self.width);
        let y = y.min(self.height);
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        let mut out = Self::new(w, h);
        for row in 0..h as usize {
            let s = ((y as usize + row) * self.width as usize + x as usize) * 4;
            let d = row * w as usize * 4;
            out.data[d..d + w as usize * 4].copy_from_slice(&self.data[s..s + w as usize * 4]);
        }
        out
    }

    /// PNG encode tuned for screenshots (see `bench_png`):
    /// * opaque images are written as RGB: ~20% smaller and faster;
    /// * fdeflate's `Fast` first (4K desktop ~30 ms); when that shows the
    ///   content compresses well (text, flat UI, gradients: under 10% of raw) it is
    ///   re-encoded with
    ///   `Balanced`, which for such content is cheap (70-140 ms) and 1.7-4x smaller.
    pub fn encode_png(&self) -> anyhow::Result<Vec<u8>> {
        let rgb = self.to_rgb_if_opaque();
        let (data, color) = match &rgb {
            Some(rgb) => (&rgb[..], png::ColorType::Rgb),
            None => (&self.data[..], png::ColorType::Rgba),
        };
        let encode = |c: png::Compression| -> anyhow::Result<Vec<u8>> {
            let mut out = Vec::with_capacity(data.len() / 8);
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(color);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(c);
            enc.write_header()?.write_image_data(data)?;
            Ok(out)
        };
        let fast = encode(png::Compression::Fast)?;
        if fast.len() * 10 < data.len() {
            let small = encode(png::Compression::Balanced)?;
            if small.len() < fast.len() {
                return Ok(small);
            }
        }
        Ok(fast)
    }

    /// Packed RGB, or None as soon as a non-opaque pixel turns up.
    fn to_rgb_if_opaque(&self) -> Option<Vec<u8>> {
        // Fixed-size blocks so the compiler drops bounds checks and
        // vectorises (4K: alpha check 2.8 -> 1.2 ms, packing 5.5 -> 4.7 ms).
        let (blocks, rest) = self.data.as_chunks::<64>();
        let mut acc = [255u8; 64];
        for b in blocks {
            for i in 0..64 {
                acc[i] &= b[i];
            }
        }
        if !(0..16).all(|i| acc[i * 4 + 3] == 255) || !rest.chunks_exact(4).all(|p| p[3] == 255) {
            return None;
        }
        let mut out = vec![0u8; self.data.len() / 4 * 3];
        let (src, srem) = self.data.as_chunks::<16>();
        let (dst, drem) = out.as_chunks_mut::<12>();
        for (o, s) in dst.iter_mut().zip(src) {
            *o = [s[0], s[1], s[2], s[4], s[5], s[6], s[8], s[9], s[10], s[12], s[13], s[14]];
        }
        for (o, s) in drem.chunks_exact_mut(3).zip(srem.chunks_exact(4)) {
            o.copy_from_slice(&s[..3]);
        }
        Some(out)
    }

    pub fn decode_png(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
        let mut reader = dec.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf)?;
        buf.truncate(info.buffer_size());
        let (w, h) = (info.width, info.height);
        let data = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            other => anyhow::bail!("unexpected png color type {other:?}"),
        };
        Ok(Self { width: w, height: h, data })
    }
}

impl Image {
    pub fn flip_vertical(&self) -> Self {
        let row = self.width as usize * 4;
        let mut out = Self::new(self.width, self.height);
        for (d, s) in out.data.chunks_exact_mut(row).zip(self.data.chunks_exact(row).rev()) {
            d.copy_from_slice(s);
        }
        out
    }

    pub fn flip_horizontal(&self) -> Self {
        let row = self.width as usize * 4;
        let mut out = self.clone();
        for r in out.data.chunks_exact_mut(row) {
            let px: &mut [[u8; 4]] = bytemuck_rows(r);
            px.reverse();
        }
        out
    }

    /// Clockwise rotation by 90, 180 or 270 degrees.
    pub fn rotate(&self, deg: u32) -> Self {
        let (w, h) = (self.width as usize, self.height as usize);
        let (nw, nh) = if deg == 180 { (w, h) } else { (h, w) };
        let mut out = Self::new(nw as u32, nh as u32);
        for y in 0..h {
            for x in 0..w {
                let (nx, ny) = match deg {
                    90 => (h - 1 - y, x),
                    180 => (w - 1 - x, h - 1 - y),
                    _ => (y, w - 1 - x),
                };
                let s = (y * w + x) * 4;
                let d = (ny * nw + nx) * 4;
                out.data[d..d + 4].copy_from_slice(&self.data[s..s + 4]);
            }
        }
        out
    }
}

fn bytemuck_rows(r: &mut [u8]) -> &mut [[u8; 4]] {
    let (pre, mid, post) = unsafe { r.align_to_mut::<[u8; 4]>() };
    debug_assert!(pre.is_empty() && post.is_empty());
    mid
}

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    fn rgb_packing_and_alpha_detection() {
        // odd pixel count exercises the remainder paths
        let mut img = Image::new(7, 3);
        for (i, p) in img.data.chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[i as u8, (i * 3) as u8, (i * 5) as u8, 255]);
        }
        let rgb = img.to_rgb_if_opaque().unwrap();
        let want: Vec<u8> = img.data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        assert_eq!(rgb, want);
        img.data[4 * 20 + 3] = 254; // last pixel, in the remainder
        assert!(img.to_rgb_if_opaque().is_none());
        img.data[4 * 20 + 3] = 255;
        img.data[3] = 0; // first pixel, in a full block
        assert!(img.to_rgb_if_opaque().is_none());
        // round trip through PNG keeps pixels
        img.data[3] = 255;
        assert_eq!(Image::decode_png(&img.encode_png().unwrap()).unwrap().data, img.data);
    }

    /// `GLINT_BENCH_PNGS=a.png,b.png cargo test --release bench_png -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_png() {
        let files = std::env::var("GLINT_BENCH_PNGS").unwrap_or_default();
        for f in files.split(',').filter(|f| !f.is_empty()) {
            let img = Image::decode_png(&std::fs::read(f).unwrap()).unwrap();
            let rgb: Vec<u8> = img.data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            let t = std::time::Instant::now();
            let chosen = img.encode_png().unwrap();
            eprintln!(
                "{f} ({}x{}): encode_png {:.1} ms {:.0} KB",
                img.width,
                img.height,
                t.elapsed().as_secs_f64() * 1e3,
                chosen.len() as f64 / 1024.0
            );
            if std::env::var_os("GLINT_BENCH_ALL").is_none() {
                continue;
            }
            for (cname, c) in [
                ("Fastest", png::Compression::Fastest),
                ("Fast", png::Compression::Fast),
                ("Balanced", png::Compression::Balanced),
                ("High", png::Compression::High),
            ] {
                for (chan, data, ct) in [("RGBA", &img.data, png::ColorType::Rgba), ("RGB", &rgb, png::ColorType::Rgb)] {
                    let mut best = f64::MAX;
                    let mut size = 0;
                    for _ in 0..3 {
                        let t = std::time::Instant::now();
                        let mut out = Vec::new();
                        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
                        enc.set_color(ct);
                        enc.set_depth(png::BitDepth::Eight);
                        enc.set_compression(c);
                        enc.write_header().unwrap().write_image_data(data).unwrap();
                        best = best.min(t.elapsed().as_secs_f64() * 1e3);
                        size = out.len();
                    }
                    eprintln!("  {cname:9} {chan:4} {best:7.1} ms {:8.0} KB", size as f64 / 1024.0);
                }
            }
        }
    }
}
