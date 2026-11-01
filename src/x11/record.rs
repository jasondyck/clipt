//! Recording on X11: frames of one monitor (or a crop of it), read with
//! MIT-SHM `GetImage` into shared memory and taken only when the DAMAGE
//! extension says the screen changed, so a still screen costs nothing. With
//! the pointer included every frame interval is taken instead, because the
//! hardware cursor moving doesn't damage the screen.

use super::X11;
use crate::capture::{LogicalRect, Output, PixelOrder, VideoFrame};
use anyhow::{Context, Result};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::ImageFormat;

impl X11 {
    /// Capture `out` continuously, handing frames to `sink` until it returns
    /// false, at most one per `interval`. `crop` is read before every frame
    /// (output-relative; X11 has no scaling, so logical = pixels).
    pub fn stream(
        &self,
        out: &Output,
        cursor: bool,
        interval: Duration,
        crop: &dyn Fn() -> Option<LogicalRect>,
        sink: &mut dyn FnMut(VideoFrame) -> bool,
    ) -> Result<()> {
        anyhow::ensure!(super::overlay::is_local_display(), "recording on X11 needs a local display (MIT-SHM)");
        let len = out.width as usize * out.height as usize * 4;
        let fd: OwnedFd = rustix::fs::memfd_create("clipt-x11-rec", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        let map = unsafe { memmap2::MmapMut::map_mut(&fd)? };
        let seg = self.conn.generate_id()?;
        self.conn.shm_attach_fd(seg, fd, false)?.check().context("MIT-SHM attach")?;
        let root = self.screen().root;
        // DAMAGE tells us when to look; without it, poll every interval.
        let damage = (|| -> Result<u32> {
            self.conn.damage_query_version(1, 1)?.reply()?;
            let id = self.conn.generate_id()?;
            self.conn.damage_create(id, root, ReportLevel::NON_EMPTY)?.check()?;
            Ok(id)
        })()
        .ok();
        let pointer = if cursor { true } else { damage.is_none() };
        let mut dirty = true;
        let mut last = Instant::now() - interval;
        loop {
            if dirty || pointer {
                let wait = interval.saturating_sub(last.elapsed());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                last = Instant::now();
                if let Some(d) = damage {
                    // Re-arm before reading, so a change during the read isn't lost.
                    self.conn.damage_subtract(d, x11rb::NONE, x11rb::NONE)?;
                }
                let frame = self.shm_frame(out, seg, &map, crop(), cursor)?;
                if !sink(frame) {
                    break;
                }
                dirty = false;
            }
            if pointer {
                while let Some(ev) = self.conn.poll_for_event()? {
                    dirty |= matches!(ev, Event::DamageNotify(_));
                }
            } else {
                let ev = self.conn.wait_for_event()?;
                dirty |= matches!(ev, Event::DamageNotify(_));
            }
        }
        if let Some(d) = damage {
            let _ = self.conn.damage_destroy(d);
        }
        let _ = self.conn.shm_detach(seg);
        let _ = self.conn.flush();
        Ok(())
    }

    fn shm_frame(&self, out: &Output, seg: u32, map: &memmap2::MmapMut, crop: Option<LogicalRect>, cursor: bool) -> Result<VideoFrame> {
        let (cx, cy, cw, ch) = crop_px(out, crop);
        let (x, y) = (out.x + cx as i32, out.y + cy as i32);
        let r = self
            .conn
            .shm_get_image(self.screen().root, x as i16, y as i16, cw as u16, ch as u16, !0, ImageFormat::Z_PIXMAP.into(), seg, 0)?
            .reply()
            .context("ShmGetImage")?;
        let visual = self.visual(r.visual).context("unknown visual")?;
        let bpp = self.bits_per_pixel(r.depth).context("unknown pixmap format")?;
        let masks = (visual.red_mask, visual.green_mask, visual.blue_mask);
        let n = cw as usize * ch as usize;
        let mut frame = match (bpp, masks) {
            // The usual layout: 32-bit pixels as B, G, R, X bytes, already packed.
            (32, (0xff0000, 0xff00, 0xff)) => VideoFrame { width: cw, height: ch, order: PixelOrder::Bgrx, data: map[..n * 4].to_vec() },
            (32, (0xff, 0xff00, 0xff0000)) => VideoFrame { width: cw, height: ch, order: PixelOrder::Rgbx, data: map[..n * 4].to_vec() },
            _ => {
                let stride = (cw as usize * bpp as usize).div_ceil(32) * 4;
                let img = super::convert(cw, ch, bpp, &visual, &map[..stride * ch as usize])?;
                VideoFrame { width: cw, height: ch, order: PixelOrder::Rgbx, data: img.data }
            }
        };
        if cursor && let Some(c) = self.cursor_image() {
            c.composite_raw(&mut frame, x, y);
        }
        Ok(frame)
    }
}

/// The crop in pixels, inside the output (whole output when `None`).
fn crop_px(out: &Output, crop: Option<LogicalRect>) -> (u32, u32, u32, u32) {
    let Some((x, y, w, h)) = crop else { return (0, 0, out.width, out.height) };
    let x = (x.round().max(0.0) as u32).min(out.width - 1);
    let y = (y.round().max(0.0) as u32).min(out.height - 1);
    (x, y, (w.round() as u32).clamp(1, out.width - x), (h.round() as u32).clamp(1, out.height - y))
}

impl super::CursorImage {
    /// Blend the pointer into a packed frame whose top-left is (ox, oy) on the root window.
    fn composite_raw(&self, f: &mut VideoFrame, ox: i32, oy: i32) {
        let (ri, bi) = match f.order {
            PixelOrder::Bgrx => (2, 0),
            PixelOrder::Rgbx => (0, 2),
        };
        for cy in 0..self.h as i32 {
            for cx in 0..self.w as i32 {
                let (x, y) = (self.x - ox + cx, self.y - oy + cy);
                if x < 0 || y < 0 || x >= f.width as i32 || y >= f.height as i32 {
                    continue;
                }
                let p = self.argb[(cy as u32 * self.w + cx as u32) as usize];
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let i = (y as usize * f.width as usize + x as usize) * 4;
                let d = &mut f.data[i..i + 4];
                // XFixes cursor pixels are premultiplied ARGB.
                let over = |src: u32, dst: u8| (src + (dst as u32 * (255 - a) + 127) / 255).min(255) as u8;
                d[ri] = over((p >> 16) & 0xff, d[ri]);
                d[1] = over((p >> 8) & 0xff, d[1]);
                d[bi] = over(p & 0xff, d[bi]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_stays_inside_the_output() {
        let o =
            Output { index: 0, name: "DP-1".into(), description: String::new(), x: 1920, y: 0, width: 1280, height: 720, native: (0, 0) };
        assert_eq!(crop_px(&o, None), (0, 0, 1280, 720));
        assert_eq!(crop_px(&o, Some((100.4, 50.6, 301.0, 200.0))), (100, 51, 301, 200));
        assert_eq!(crop_px(&o, Some((1200.0, 700.0, 500.0, 500.0))), (1200, 700, 80, 20));
    }
}
