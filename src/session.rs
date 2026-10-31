//! What the interactive flow needs from a display system. Implemented by the
//! Wayland overlay (`overlay.rs`) and the X11 one (`x11/overlay.rs`).

use crate::editor::{Editor, Outcome};
use anyhow::Result;
use std::sync::{Arc, Condvar, Mutex};

pub trait UiSession {
    /// Show the editor until the user copies, saves or cancels.
    fn run(&mut self) -> Result<Outcome>;
    fn editor(&mut self) -> &mut Editor;
    /// Take ownership of the clipboard for a PNG that is still being encoded.
    /// Wayland needs this while the overlay still has input focus.
    fn claim_clipboard(&mut self) -> Result<Arc<Later<Vec<u8>>>>;
    /// Close the overlay but keep the connection (and clipboard) alive.
    fn hide(&mut self) -> Result<()>;
    /// Drop captures and buffers before lingering as clipboard owner.
    fn release_memory(&mut self);
    /// Answer paste requests until another client takes the clipboard.
    fn serve_clipboard(&mut self) -> Result<()>;
}

/// Open the editor on whichever display system clipt is running under.
pub fn open(editor: Editor, display: crate::capture::DisplayKind) -> Result<Box<dyn UiSession>> {
    Ok(match display {
        crate::capture::DisplayKind::Wayland => Box::new(crate::overlay::Session::new(editor)?),
        crate::capture::DisplayKind::X11 => Box::new(crate::x11::overlay::Session::new(editor)?),
    })
}

/// A value filled in later by one thread and awaited by others.
pub struct Later<T> {
    value: Mutex<Option<Arc<T>>>,
    ready: Condvar,
}

impl<T> Default for Later<T> {
    fn default() -> Self {
        Self { value: Mutex::new(None), ready: Condvar::new() }
    }
}

impl<T> Later<T> {
    pub fn set(&self, v: T) {
        *self.value.lock().unwrap() = Some(Arc::new(v));
        self.ready.notify_all();
    }
    pub fn wait(&self) -> Arc<T> {
        let mut g = self.value.lock().unwrap();
        loop {
            if let Some(v) = &*g {
                return v.clone();
            }
            g = self.ready.wait(g).unwrap();
        }
    }
}
