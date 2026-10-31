//! Owning the X11 CLIPBOARD selection and answering paste requests (ICCCM),
//! including the INCR protocol for PNGs larger than one request.

use super::{Atoms, X11};
use crate::session::Later;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

pub struct ClipServer {
    /// Unmapped window that owns the selection; outlives the overlay.
    pub window: Window,
    data: Arc<Later<Vec<u8>>>,
    /// In-progress INCR transfers: (requestor, property) -> bytes sent.
    incr: HashMap<(Window, u32), usize>,
    pub done: bool,
}

impl ClipServer {
    /// Create the owner window and claim CLIPBOARD. `time` should be the
    /// timestamp of the user action (CURRENT_TIME is tolerated).
    pub fn claim(x: &X11, data: Arc<Later<Vec<u8>>>, time: u32) -> Result<Self> {
        let window = x.conn.generate_id()?;
        let root = x.screen().root;
        x.conn.create_window(
            0,
            window,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        x.conn.set_selection_owner(window, x.atoms.CLIPBOARD, time)?;
        let owner = x.conn.get_selection_owner(x.atoms.CLIPBOARD)?.reply()?.owner;
        anyhow::ensure!(owner == window, "another client kept the X11 clipboard");
        Ok(Self { window, data, incr: HashMap::new(), done: false })
    }

    /// Handle one event; returns true if it was ours.
    pub fn handle(&mut self, x: &X11, ev: &Event) -> Result<bool> {
        match ev {
            Event::SelectionRequest(r) if r.owner == self.window => {
                self.request(x, r)?;
                Ok(true)
            }
            Event::SelectionClear(c) if c.owner == self.window && c.selection == x.atoms.CLIPBOARD => {
                self.done = true;
                Ok(true)
            }
            Event::PropertyNotify(p) if p.state == Property::DELETE => {
                let key = (p.window, p.atom);
                if let Some(&sent) = self.incr.get(&key) {
                    self.incr_chunk(x, key, sent)?;
                    return Ok(true);
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn request(&mut self, x: &X11, r: &SelectionRequestEvent) -> Result<()> {
        let a: &Atoms = &x.atoms;
        // Obsolete clients send property None: use the target as property.
        let prop = if r.property == u32::from(AtomEnum::NONE) { r.target } else { r.property };
        let ok = if r.target == a.TARGETS {
            x.conn.change_property32(PropMode::REPLACE, r.requestor, prop, a.ATOM, &[a.TARGETS, a.TIMESTAMP, a.PNG])?;
            true
        } else if r.target == a.PNG {
            let data = self.data.wait();
            if data.len() <= max_chunk(x) {
                x.conn.change_property8(PropMode::REPLACE, r.requestor, prop, a.PNG, &data)?;
            } else {
                // INCR: announce the size, then send chunks each time the
                // requestor deletes the property.
                x.conn.change_window_attributes(r.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
                x.conn.change_property32(PropMode::REPLACE, r.requestor, prop, a.INCR, &[data.len() as u32])?;
                self.incr.insert((r.requestor, prop), 0);
            }
            true
        } else {
            false
        };
        let ev = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property: if ok { prop } else { AtomEnum::NONE.into() },
        };
        x.conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
        x.conn.flush()?;
        Ok(())
    }

    fn incr_chunk(&mut self, x: &X11, key: (Window, u32), sent: usize) -> Result<()> {
        let data = self.data.wait();
        let end = (sent + max_chunk(x)).min(data.len());
        // A zero-length write after the last chunk ends the transfer.
        x.conn.change_property8(PropMode::REPLACE, key.0, key.1, x.atoms.PNG, &data[sent..end])?;
        if sent == data.len() {
            self.incr.remove(&key);
            x.conn.change_window_attributes(key.0, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT))?;
        } else {
            self.incr.insert(key, end);
        }
        x.conn.flush()?;
        Ok(())
    }
}

/// Largest property payload we send in one request.
fn max_chunk(x: &X11) -> usize {
    (x.conn.maximum_request_bytes() / 4).clamp(64 * 1024, 4 * 1024 * 1024)
}

/// Headless copy (`clipt full -c`): own the clipboard and serve until replaced.
pub fn copy_and_serve(png: Vec<u8>, before_serving: impl FnOnce()) -> Result<()> {
    let x = X11::connect()?;
    let data = Arc::new(Later::default());
    data.set(png);
    let mut server = ClipServer::claim(&x, data, x11rb::CURRENT_TIME)?;
    x.conn.flush()?;
    before_serving();
    while !server.done {
        let ev = x.conn.wait_for_event()?;
        server.handle(&x, &ev)?;
    }
    Ok(())
}
