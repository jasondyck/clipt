//! Following a window by cropping its output, where the compositor can't
//! capture single windows but says where they are (Sway and Hyprland IPC,
//! the EWMH window list on X11).
//! Polled ten times a second; the camera smooths the steps. Unlike window
//! capture, whatever covers the window is recorded too.

use super::STOP;
use crate::capture::{LogicalRect, Output};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The window list can be asked again for a window's position.
pub fn supported() -> bool {
    std::env::var_os("SWAYSOCK").is_some() || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// Keep `crop` on window `id` (clipped to `out`) until the recording stops.
/// A window that closes or leaves the output keeps its last crop.
pub fn run(id: &str, out: &Output, crop: &Arc<Mutex<Option<LogicalRect>>>, x11: bool) {
    while !STOP.load(Ordering::SeqCst) {
        if let Some(w) = crate::windows::list(x11).into_iter().find(|w| w.id == id)
            && let Some(r) = clip(out, (w.x as f64, w.y as f64, w.width as f64, w.height as f64))
        {
            *crop.lock().unwrap() = Some(r);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A desktop rectangle as an output-relative crop, clipped to the output.
fn clip(out: &Output, (x, y, w, h): LogicalRect) -> Option<LogicalRect> {
    let (ox, oy, ow, oh) = (out.x as f64, out.y as f64, out.width as f64, out.height as f64);
    let (l, t) = (x.max(ox), y.max(oy));
    let (r, b) = ((x + w).min(ox + ow), (y + h).min(oy + oh));
    (r - l >= 2.0 && b - t >= 2.0).then_some((l - ox, t - oy, r - l, b - t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_to_the_output() {
        let o = Output { index: 0, name: "B".into(), description: String::new(), x: 1280, y: 0, width: 1920, height: 1080, native: (0, 0) };
        assert_eq!(clip(&o, (1300.0, 100.0, 400.0, 300.0)), Some((20.0, 100.0, 400.0, 300.0)));
        assert_eq!(clip(&o, (1200.0, 100.0, 400.0, 300.0)), Some((0.0, 100.0, 320.0, 300.0)));
        assert_eq!(clip(&o, (0.0, 0.0, 100.0, 100.0)), None, "on another output");
    }
}
