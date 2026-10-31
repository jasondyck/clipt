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
    pub fn from_rows(
        width: u32,
        height: u32,
        stride: usize,
        bpp: usize,
        src: &[u8],
        f: impl Fn(&[u8]) -> [u8; 4],
    ) -> anyhow::Result<Self> {
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

    pub fn encode_png(&self) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.data.len() / 3);
        {
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            // Fast filter + default zlib keeps big 4K captures under ~100ms.
            enc.set_compression(png::Compression::Fast);
            let mut w = enc.write_header()?;
            w.write_image_data(&self.data)?;
        }
        Ok(out)
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
