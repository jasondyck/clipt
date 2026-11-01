//! Where the windows are, so the overlay can highlight the one under the
//! pointer and select it with a click.
//!
//! Wayland has no protocol for this, so each compositor is asked in its own
//! way: KWin through a throwaway script, Sway and Hyprland over their IPC
//! sockets, and X11 through the EWMH window list. Anything else (GNOME, which
//! doesn't expose window positions to clients) returns an empty list and the
//! overlay falls back to highlighting whole monitors.
//!
//! Positions are in the same logical desktop coordinates as the outputs. The
//! list is topmost first.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Window {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// App id / WM_CLASS, for the label when there's no title.
    pub app: String,
    pub title: String,
    /// The compositor's own id, for finding the window again while
    /// recording it: Sway's container id, Hyprland's address, KWin's
    /// internal id (a UUID). Empty elsewhere.
    pub id: String,
}

impl Window {
    /// What the hover label shows: the app, since titles are often long.
    pub fn name(&self) -> &str {
        if self.app.is_empty() { &self.title } else { &self.app }
    }
}

/// Visible windows, topmost first. Never fails: a desktop we can't ask just
/// gets no window highlighting.
pub fn list(x11: bool) -> Vec<Window> {
    let t0 = std::time::Instant::now();
    let r = if x11 {
        x11_windows()
    } else if std::env::var_os("SWAYSOCK").is_some() {
        sway()
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        hyprland()
    } else if is_kwin() {
        kwin()
    } else {
        Ok(Vec::new())
    };
    let wins = r.unwrap_or_else(|e| {
        eprintln!("clipt: window list unavailable: {e:#}");
        Vec::new()
    });
    if std::env::var_os("CLIPT_DEBUG").is_some() {
        eprintln!("clipt: {} windows in {:.1} ms", wins.len(), t0.elapsed().as_secs_f64() * 1e3);
        for w in &wins {
            eprintln!("clipt:   {:>6.0},{:<6.0} {:>5.0}x{:<5.0} {} — {}", w.x, w.y, w.width, w.height, w.app, w.title);
        }
    }
    wins.into_iter().filter(|w| w.width >= 2.0 && w.height >= 2.0).collect()
}

fn is_kwin() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")))
}

// ---- KWin -----------------------------------------------------------------

/// KWin only answers window questions from inside a script, so load one that
/// calls back with the visible windows and unload it again (~10-20 ms).
fn kwin() -> Result<Vec<Window>> {
    use std::sync::{Mutex, mpsc};

    struct Sink(Mutex<Option<mpsc::Sender<String>>>);
    #[zbus::interface(name = "org.clipt.Windows")]
    impl Sink {
        fn report(&self, json: String) {
            if let Some(tx) = self.0.lock().unwrap().take() {
                let _ = tx.send(json);
            }
        }
    }

    let (tx, rx) = mpsc::channel();
    let conn = zbus::block_on(crate::dbus::builder().serve_at("/clipt/Windows", Sink(Mutex::new(Some(tx)))).build())?;
    let me = conn.unique_name().context("no unique bus name")?.to_string();

    // Named after the bus connection: in a Flatpak every clipt is PID 2.
    let plugin = format!("clipt-windows-{}", me.trim_start_matches(':').replace('.', "_"));
    let mut dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    // KWin reads the file from outside the sandbox; of the runtime dir, a
    // Flatpak only shares app/<id>/ with the host, at the same path.
    if let (true, Some(id)) = (crate::update::flatpak(), std::env::var_os("FLATPAK_ID")) {
        dir = dir.join("app").join(id);
    }
    let path = dir.join(format!("{plugin}.js"));
    std::fs::write(&path, KWIN_SCRIPT.replace("@DEST@", &me))?;

    let scripting = crate::dbus::Proxy::new(&conn, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting")?;
    let result = (|| -> Result<String> {
        let id: i32 = scripting.call("loadScript", &(path.to_string_lossy().as_ref(), plugin.as_str()))?;
        anyhow::ensure!(id >= 0, "KWin refused the script");
        let script_path = format!("/Scripting/Script{id}");
        let script = crate::dbus::Proxy::new(&conn, "org.kde.KWin", &script_path, "org.kde.kwin.Script")?;
        script.call::<_, ()>("run", &())?;
        rx.recv_timeout(Duration::from_millis(500)).context("KWin script didn't report back")
    })();
    let _: zbus::Result<bool> = scripting.call("unloadScript", &(plugin.as_str(),));
    let _ = std::fs::remove_file(&path);

    #[derive(Deserialize)]
    struct W {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        app: String,
        title: String,
        #[serde(default)]
        id: String,
    }
    let ws: Vec<W> = serde_json::from_str(&result?)?;
    Ok(ws
        .into_iter()
        .map(|w| Window { x: w.x.round(), y: w.y.round(), width: w.w.round(), height: w.h.round(), app: w.app, title: w.title, id: w.id })
        .collect())
}

const KWIN_SCRIPT: &str = r#"
const desk = workspace.currentDesktop;
const act = workspace.currentActivity;
const out = [];
const order = workspace.stackingOrder;
for (let i = order.length - 1; i >= 0; i--) {
    const w = order[i];
    if (!(w.normalWindow || w.dialog) || w.minimized || w.hidden) continue;
    if (!w.onAllDesktops && !w.desktops.some(d => d.id === desk.id)) continue;
    if (w.activities.length && w.activities.indexOf(act) < 0) continue;
    const g = w.frameGeometry;
    out.push({x: g.x, y: g.y, w: g.width, h: g.height, app: w.resourceClass || "", title: w.caption || "", id: String(w.internalId)});
}
callDBus("@DEST@", "/clipt/Windows", "org.clipt.Windows", "Report", JSON.stringify(out));
"#;

// ---- Sway (i3 IPC) ----------------------------------------------------------

fn i3_ipc(sock: &str, kind: u32) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut msg = b"i3-ipc".to_vec();
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    s.write_all(&msg)?;
    let mut hdr = [0u8; 14];
    s.read_exact(&mut hdr)?;
    anyhow::ensure!(&hdr[..6] == b"i3-ipc", "bad IPC reply");
    let len = u32::from_ne_bytes(hdr[6..10].try_into()?) as usize;
    let mut body = vec![0; len];
    s.read_exact(&mut body)?;
    Ok(body)
}

fn sway() -> Result<Vec<Window>> {
    #[derive(Deserialize)]
    struct R {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    }
    #[derive(Deserialize)]
    struct Props {
        class: Option<String>,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "type")]
        kind: String,
        rect: R,
        name: Option<String>,
        id: i64,
        app_id: Option<String>,
        window_properties: Option<Props>,
        pid: Option<i64>,
        visible: Option<bool>,
        #[serde(default)]
        nodes: Vec<Node>,
        #[serde(default)]
        floating_nodes: Vec<Node>,
    }

    fn walk(n: &Node, floating: bool, tiled: &mut Vec<Window>, floats: &mut Vec<Window>) {
        let is_view = n.pid.is_some() && n.nodes.is_empty() && matches!(n.kind.as_str(), "con" | "floating_con");
        if is_view && n.visible == Some(true) {
            let app = n.app_id.clone().or_else(|| n.window_properties.as_ref().and_then(|p| p.class.clone())).unwrap_or_default();
            let w = Window {
                x: n.rect.x,
                y: n.rect.y,
                width: n.rect.width,
                height: n.rect.height,
                app,
                title: n.name.clone().unwrap_or_default(),
                id: n.id.to_string(),
            };
            if floating { floats.push(w) } else { tiled.push(w) }
        }
        for c in &n.nodes {
            walk(c, floating, tiled, floats);
        }
        for c in &n.floating_nodes {
            walk(c, true, tiled, floats);
        }
    }

    let sock = std::env::var("SWAYSOCK")?;
    let tree: Node = serde_json::from_slice(&i3_ipc(&sock, 4)?)?;
    let (mut tiled, mut floats) = (Vec::new(), Vec::new());
    walk(&tree, false, &mut tiled, &mut floats);
    // Floating windows sit above tiled ones; later floating nodes are on top.
    floats.reverse();
    floats.extend(tiled);
    Ok(floats)
}

// ---- Hyprland ---------------------------------------------------------------

fn hyprland() -> Result<Vec<Window>> {
    use std::io::{Read, Write};
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let ask = |cmd: &str| -> Result<Vec<u8>> {
        let path = [format!("{runtime}/hypr/{sig}/.socket.sock"), format!("/tmp/hypr/{sig}/.socket.sock")]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .context("no Hyprland socket")?;
        let mut s = std::os::unix::net::UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_millis(500)))?;
        s.write_all(cmd.as_bytes())?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)?;
        Ok(out)
    };

    #[derive(Deserialize)]
    struct Ws {
        id: i64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Monitor {
        active_workspace: Ws,
        special_workspace: Option<Ws>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Client {
        at: [f32; 2],
        size: [f32; 2],
        workspace: Ws,
        #[serde(default)]
        mapped: bool,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        floating: bool,
        #[serde(default)]
        fullscreen: serde_json::Value,
        #[serde(default)]
        focus_history_id: i64,
        #[serde(default)]
        class: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        address: String,
    }

    let mons: Vec<Monitor> = serde_json::from_slice(&ask("j/monitors")?)?;
    let shown: Vec<i64> = mons
        .iter()
        .flat_map(|m| [Some(m.active_workspace.id), m.special_workspace.as_ref().map(|w| w.id)])
        .flatten()
        .filter(|&id| id != 0)
        .collect();
    let mut cs: Vec<Client> = serde_json::from_slice(&ask("j/clients")?)?;
    cs.retain(|c| c.mapped && !c.hidden && shown.contains(&c.workspace.id));
    // Fullscreen, then floating, then tiled; most recently focused first.
    let full = |c: &Client| c.fullscreen.as_bool().unwrap_or(false) || c.fullscreen.as_i64().unwrap_or(0) > 0;
    cs.sort_by_key(|c| (!full(c), !c.floating, c.focus_history_id));
    Ok(cs
        .into_iter()
        .map(|c| Window { x: c.at[0], y: c.at[1], width: c.size[0], height: c.size[1], app: c.class, title: c.title, id: c.address })
        .collect())
}

// ---- X11 (EWMH) -------------------------------------------------------------

fn x11_windows() -> Result<Vec<Window>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, MapState, Window as Xid};

    x11rb::atom_manager! {
        A: ACookie {
            _NET_CLIENT_LIST_STACKING,
            _NET_CURRENT_DESKTOP,
            _NET_WM_DESKTOP,
            _NET_WM_STATE,
            _NET_WM_STATE_HIDDEN,
            _NET_WM_WINDOW_TYPE,
            _NET_WM_WINDOW_TYPE_DOCK,
            _NET_WM_WINDOW_TYPE_DESKTOP,
            _NET_FRAME_EXTENTS,
            _GTK_FRAME_EXTENTS,
            _NET_WM_NAME,
            UTF8_STRING,
        }
    }

    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let a = A::new(&conn)?.reply()?;
    let cards = |w: Xid, prop: Atom, ty: AtomEnum| -> Vec<u32> {
        conn.get_property(false, w, prop, ty, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    };
    let stack = cards(root, a._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW);
    // Empty both when nothing is open and when the window manager doesn't
    // do EWMH; either way there's nothing to highlight.
    if stack.is_empty() {
        return Ok(Vec::new());
    }
    let desk = cards(root, a._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL).first().copied();

    let mut out = Vec::new();
    for &w in stack.iter().rev() {
        let Ok(attrs) = conn.get_window_attributes(w)?.reply() else { continue };
        if attrs.map_state != MapState::VIEWABLE {
            continue;
        }
        if cards(w, a._NET_WM_STATE, AtomEnum::ATOM).contains(&a._NET_WM_STATE_HIDDEN) {
            continue;
        }
        let types = cards(w, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM);
        if types.contains(&a._NET_WM_WINDOW_TYPE_DOCK) || types.contains(&a._NET_WM_WINDOW_TYPE_DESKTOP) {
            continue;
        }
        if let (Some(d), Some(&wd)) = (desk, cards(w, a._NET_WM_DESKTOP, AtomEnum::CARDINAL).first())
            && wd != d
            && wd != u32::MAX
        {
            continue;
        }
        let Ok(geo) = conn.get_geometry(w)?.reply() else { continue };
        let Ok(pos) = conn.translate_coordinates(w, root, 0, 0)?.reply() else { continue };
        let (mut x, mut y, mut ww, mut hh) = (pos.dst_x as f32, pos.dst_y as f32, geo.width as f32, geo.height as f32);
        // Server-side decorations (left, right, top, bottom) are outside the client...
        if let [l, r, t, b] = cards(w, a._NET_FRAME_EXTENTS, AtomEnum::CARDINAL)[..] {
            (x, y, ww, hh) = (x - l as f32, y - t as f32, ww + (l + r) as f32, hh + (t + b) as f32);
        }
        // ...and client-side shadows are inside it.
        if let [l, r, t, b] = cards(w, a._GTK_FRAME_EXTENTS, AtomEnum::CARDINAL)[..] {
            (x, y, ww, hh) = (x + l as f32, y + t as f32, ww - (l + r) as f32, hh - (t + b) as f32);
        }
        let title = conn
            .get_property(false, w, a._NET_WM_NAME, a.UTF8_STRING, 0, 256)?
            .reply()
            .ok()
            .map(|r| String::from_utf8_lossy(&r.value).into_owned())
            .unwrap_or_default();
        let app = conn
            .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?
            .reply()
            .ok()
            .and_then(|r| r.value.split(|&b| b == 0).filter(|s| !s.is_empty()).nth(1).map(|s| String::from_utf8_lossy(s).into_owned()))
            .unwrap_or_default();
        out.push(Window { x, y, width: ww, height: hh, app, title, id: String::new() });
    }
    Ok(out)
}
