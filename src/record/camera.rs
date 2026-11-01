//! The virtual camera for window recordings: frames of a changing size go
//! into a video of fixed size.
//!
//! The canvas is the window's size when recording starts. Each frame the
//! target zoom is the largest that fits the window in the canvas without
//! enlarging it past 1:1 (so text stays sharp); the zoom eases toward it with
//! a critically damped spring (~200 ms, no overshoot), so a resize reads as a
//! smooth zoom instead of a jump. The window is centred on a dark background.

use crate::capture::VideoFrame;
use std::time::Instant;

/// Spring stiffness: settles to within 2 % in about 5.8 / OMEGA seconds.
const OMEGA: f32 = 28.0;
const BG: [u8; 4] = [0x1e, 0x1e, 0x22, 0xff];

pub struct Camera {
    pub width: u32,
    pub height: u32,
    scale: f32,
    vel: f32,
    last: Option<Instant>,
}

impl Camera {
    /// A canvas for frames like `w` x `h` (rounded up to even sizes, which
    /// H.264 needs).
    pub fn new(w: u32, h: u32) -> Self {
        Self { width: w + w % 2, height: h + h % 2, scale: 1.0, vel: 0.0, last: None }
    }

    pub fn target(&self, fw: u32, fh: u32) -> f32 {
        (self.width as f32 / fw.max(1) as f32).min(self.height as f32 / fh.max(1) as f32).min(1.0)
    }

    /// Advance the zoom to `now` for a frame of `fw` x `fh`.
    pub fn step(&mut self, fw: u32, fh: u32, now: Instant) {
        let target = self.target(fw, fh);
        let Some(last) = self.last.replace(now) else {
            self.scale = target;
            return;
        };
        // At most one frame interval's worth: after a still stretch the last
        // step was long ago, and the glide should start now, not be over.
        let mut dt = now.saturating_duration_since(last).as_secs_f32().min(0.05);
        // Small fixed steps keep the integration stable at any frame rate.
        while dt > 0.0 {
            let h = dt.min(1.0 / 240.0);
            let acc = OMEGA * OMEGA * (target - self.scale) - 2.0 * OMEGA * self.vel;
            self.vel += acc * h;
            self.scale += self.vel * h;
            dt -= h;
        }
        // Snap once the rest of the glide is under a couple of pixels.
        if (self.scale - target).abs() < 4e-3 && self.vel.abs() < 5e-2 {
            self.scale = target;
            self.vel = 0.0;
        }
    }

    /// Still easing toward the zoom a `fw` x `fh` frame needs.
    pub fn animating(&self, fw: u32, fh: u32) -> bool {
        self.scale != self.target(fw, fh)
    }

    #[cfg(test)]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Draw `f` into a canvas frame at the current zoom.
    pub fn render(&self, f: &VideoFrame) -> VideoFrame {
        let (cw, ch) = (self.width as usize, self.height as usize);
        let mut data = Vec::with_capacity(cw * ch * 4);
        for _ in 0..cw * ch {
            data.extend_from_slice(&BG);
        }
        let s = self.scale;
        let (sw, sh) = (f.width as f32 * s, f.height as f32 * s);
        let (ox, oy) = ((cw as f32 - sw) / 2.0, (ch as f32 - sh) / 2.0);
        if (s - 1.0).abs() < 1e-4 {
            copy_centered(f, &mut data, cw, ch, ox.round() as i64, oy.round() as i64);
        } else {
            scale_into(f, &mut data, cw, ch, ox, oy, s);
        }
        VideoFrame { width: self.width, height: self.height, order: f.order, data }
    }
}

/// 1:1: copy rows of `f` with its top-left at (ox, oy), clipped to the canvas.
fn copy_centered(f: &VideoFrame, dst: &mut [u8], cw: usize, ch: usize, ox: i64, oy: i64) {
    let (fw, fh) = (f.width as i64, f.height as i64);
    let (x0, x1) = (ox.max(0), (ox + fw).min(cw as i64));
    let (y0, y1) = (oy.max(0), (oy + fh).min(ch as i64));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let n = (x1 - x0) as usize * 4;
    for y in y0..y1 {
        let s = ((y - oy) * fw + (x0 - ox)) as usize * 4;
        let d = (y as usize * cw + x0 as usize) * 4;
        dst[d..d + n].copy_from_slice(&f.data[s..s + n]);
    }
}

/// Bilinear: canvas pixel (x, y) shows source ((x - ox) / s, (y - oy) / s).
/// Separable and fixed point (8-bit weights); large frames use 4 threads.
fn scale_into(f: &VideoFrame, dst: &mut [u8], cw: usize, ch: usize, ox: f32, oy: f32, s: f32) {
    let (fw, fh) = (f.width as usize, f.height as usize);
    if fw < 2 || fh < 2 {
        return;
    }
    let x0 = ox.max(0.0).floor() as usize;
    let x1 = ((ox + fw as f32 * s).ceil().max(0.0) as usize).min(cw);
    let y0 = oy.max(0.0).floor() as usize;
    let y1 = ((oy + fh as f32 * s).ceil().max(0.0) as usize).min(ch);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let tap = |d: usize, o: f32, n: usize| -> (usize, u32) {
        let v = ((d as f32 + 0.5 - o) / s - 0.5).clamp(0.0, (n - 1) as f32);
        let i = (v as usize).min(n - 2);
        (i, ((v - i as f32) * 256.0) as u32)
    };
    let xs: Vec<(usize, u32)> = (x0..x1).map(|x| tap(x, ox, fw)).collect();
    let w = x1 - x0;
    let threads = if w * (y1 - y0) > 1_000_000 { 4 } else { 1 };
    let rows_per = (y1 - y0).div_ceil(threads);
    let src = &f.data;
    let region = &mut dst[y0 * cw * 4..y1 * cw * 4];
    std::thread::scope(|sc| {
        for (t, chunk) in region.chunks_mut(rows_per * cw * 4).enumerate() {
            let xs = &xs;
            let mut work = move || {
                let mut a = vec![0u32; w * 4];
                let mut b = vec![0u32; w * 4];
                for (r, row) in chunk.chunks_exact_mut(cw * 4).enumerate() {
                    let (j, wy) = tap(y0 + t * rows_per + r, oy, fh);
                    for (line, acc) in [(j, &mut a), (j + 1, &mut b)] {
                        let l = &src[line * fw * 4..(line + 1) * fw * 4];
                        for (k, &(i, wx)) in xs.iter().enumerate() {
                            let p = &l[i * 4..i * 4 + 8];
                            for c in 0..4 {
                                acc[k * 4 + c] = p[c] as u32 * (256 - wx) + p[c + 4] as u32 * wx;
                            }
                        }
                    }
                    let out = &mut row[x0 * 4..x1 * 4];
                    for k in 0..w * 4 {
                        out[k] = ((a[k] * (256 - wy) + b[k] * wy) >> 16) as u8;
                    }
                }
            };
            if threads == 1 {
                work();
            } else {
                sc.spawn(work);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::PixelOrder;
    use std::time::Duration;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> VideoFrame {
        VideoFrame { width: w, height: h, order: PixelOrder::Bgrx, data: px.repeat((w * h) as usize) }
    }

    /// Bounding box of pixels that aren't the background.
    fn content(f: &VideoFrame) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for (i, p) in f.data.chunks_exact(4).enumerate() {
            if p != BG {
                let (x, y) = (i as u32 % f.width, i as u32 / f.width);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
        (x0, y0, x1 - x0, y1 - y0)
    }

    #[test]
    fn fits_without_enlarging() {
        let c = Camera::new(400, 300);
        assert_eq!(c.target(400, 300), 1.0);
        assert_eq!(c.target(200, 100), 1.0, "smaller windows stay 1:1");
        assert_eq!(c.target(800, 300), 0.5);
        assert_eq!(c.target(400, 600), 0.5);
        assert_eq!((Camera::new(401, 301).width, Camera::new(401, 301).height), (402, 302));
    }

    #[test]
    fn spring_eases_without_overshoot() {
        let mut c = Camera::new(400, 300);
        let t = Instant::now();
        c.step(400, 300, t);
        assert_eq!(c.scale(), 1.0);
        // The window doubles in width: the zoom glides to 0.5.
        let mut prev = 1.0;
        let mut steps = 0;
        for i in 1..=30 {
            c.step(800, 300, t + Duration::from_millis(i * 16));
            assert!(c.scale() <= prev + 1e-6, "monotonic");
            assert!(c.scale() >= 0.5 - 1e-6, "no overshoot");
            if c.scale() > 0.5 {
                steps += 1;
            }
            prev = c.scale();
        }
        assert!((4..=20).contains(&steps), "settles in ~100-300 ms, took {steps} frames");
        assert!(!c.animating(800, 300));
    }

    #[test]
    fn glide_starts_at_the_resize_after_a_still_stretch() {
        let mut c = Camera::new(400, 300);
        let t = Instant::now();
        c.step(400, 300, t);
        // nothing for two seconds, then the window doubles in width
        c.step(800, 300, t + Duration::from_secs(2));
        assert!(c.scale() > 0.75, "first frame after the resize barely moves, got {}", c.scale());
        assert!(c.animating(800, 300));
    }

    #[test]
    fn renders_centred_at_one_to_one_and_scaled() {
        let px = [10, 200, 30, 255];
        let mut c = Camera::new(400, 300);
        let t = Instant::now();
        c.step(200, 100, t);
        let out = c.render(&solid(200, 100, px));
        assert_eq!((out.width, out.height), (400, 300));
        assert_eq!(content(&out), (100, 100, 200, 100));
        // a settled 0.5 zoom of an 800x600 window fills the canvas
        let mut c = Camera::new(400, 300);
        c.step(800, 600, t);
        let out = c.render(&solid(800, 600, px));
        assert_eq!(content(&out), (0, 0, 400, 300));
        assert_eq!(&out.data[(150 * 400 + 200) * 4..][..4], &px);
        // 800x300 at 0.5: 400x150, centred vertically
        let mut c = Camera::new(400, 300);
        c.step(800, 300, t);
        let (x, y, w, h) = content(&c.render(&solid(800, 300, px)));
        assert_eq!((x, w), (0, 400));
        assert!((74..=76).contains(&y) && (149..=151).contains(&h), "{y} {h}");
    }

    #[test]
    fn mid_zoom_larger_than_canvas_is_clipped() {
        let mut c = Camera::new(400, 300);
        let t = Instant::now();
        c.step(400, 300, t);
        c.step(800, 600, t + Duration::from_millis(16)); // still near 1.0: frame bigger than canvas
        assert!(c.scale() > 0.9);
        let out = c.render(&solid(800, 600, [1, 2, 3, 255]));
        assert_eq!(out.data.len(), 400 * 300 * 4);
    }
}
